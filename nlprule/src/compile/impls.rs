use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

use crate::{
    rule::{
        disambiguation::POSFilter,
        engine::composition::{Matcher, PosMatcher, TextMatcher},
        DisambiguationRule, MatchGraph, Rule,
    },
    rules::{Rules, RulesOptions},
    tokenizer::{chunk, Tokenizer, TokenizerOptions},
    types::*,
    utils::parallelism::MaybeParallelIterator,
};

use super::parse_structure::BuildInfo;

impl TextMatcher {
    pub fn new(matcher: Matcher, info: &mut BuildInfo) -> Self {
        let graph = MatchGraph::default();

        let set = if matcher.needs_graph() {
            None
        } else if let either::Right(regex) = &matcher.matcher {
            let mut hasher = DefaultHasher::default();
            regex.hash(&mut hasher);
            matcher.negate.hash(&mut hasher);
            matcher.empty_always_false.hash(&mut hasher);
            let matcher_hash = hasher.finish();

            if let Some(set) = info.mut_regex_cache().get(&matcher_hash) {
                set.clone()
            } else {
                let data: Vec<_> = info.tagger().word_store().iter().collect();

                let set: DefaultHashSet<u32> = data
                    .into_maybe_par_iter()
                    .filter_map(|(word, id)| {
                        if matcher.is_match(word.as_str(), &graph, None) {
                            Some(*id)
                        } else {
                            None
                        }
                    })
                    .collect();

                // there are some regexes which match lots of strings
                // this cutoff is pretty arbitrary but without any threshold the size of some sets blows up
                // the vast majority of regexes matches less than 100 strings from manual inspection
                let set = if set.len() > 100 { None } else { Some(set) };
                info.mut_regex_cache().insert(matcher_hash, set.clone());
                set
            }
        } else {
            None
        };

        TextMatcher { matcher, set }
    }
}

impl PosMatcher {
    pub fn new(matcher: Matcher, info: &mut BuildInfo) -> Self {
        let mut mask = vec![false; info.tagger().tag_store().len()];
        let graph = MatchGraph::default();

        for (word, id) in info.tagger().tag_store().iter() {
            mask[*id as usize] = matcher.is_match(word.as_str(), &graph, None);
        }

        PosMatcher { mask }
    }
}

impl Rules {
    pub fn from_xml<P: AsRef<std::path::Path>>(
        path: P,
        build_info: &mut BuildInfo,
        options: RulesOptions,
    ) -> Self {
        use log::warn;
        use std::collections::HashMap;

        let rules = super::parse_structure::read_rules(path);
        let mut errors: HashMap<String, usize> = HashMap::new();

        let rules: Vec<_> = rules
            .into_iter()
            .filter_map(|x| match x {
                Ok((rule_structure, group, category)) => {
                    let id = rule_structure.id.as_ref().map_or_else(
                        || {
                            let group = group.as_ref().expect("must have group if ID not set");
                            format!("{}.{}", group.id, group.n)
                        },
                        |x| x.clone(),
                    );
                    let category = category.expect("grammar rules must have category");
                    let off = rule_structure
                        .default
                        .as_ref()
                        .map(|x| x == "off")
                        .or_else(|| {
                            group
                                .as_ref()
                                .and_then(|x| x.default.as_ref().map(|x| x == "off"))
                        })
                        .or_else(|| category.default.as_ref().map(|x| x == "off"))
                        .unwrap_or(false);
                    let name = rule_structure.name.as_ref().map_or_else(
                        || {
                            let group = group.as_ref().expect("must have group if name not set");
                            group.name.clone()
                        },
                        |x| x.clone(),
                    );

                    match Rule::from_rule_structure(rule_structure, build_info) {
                        Ok(mut rule) => {
                            if (options.ids.is_empty() || options.ids.contains(&id))
                                && !options.ignore_ids.contains(&id)
                            {
                                rule.id = id;
                                rule.name = name;
                                rule.on = !off;
                                rule.category_id = category.id;
                                rule.category_name = category.name;
                                rule.category_type = category.kind;
                                Some(rule)
                            } else {
                                None
                            }
                        }
                        Err(x) => {
                            *errors.entry(format!("[Rule] {}", x)).or_insert(0) += 1;
                            None
                        }
                    }
                }
                Err(x) => {
                    *errors.entry(format!("[Structure] {}", x)).or_insert(0) += 1;
                    None
                }
            })
            .collect();

        if !errors.is_empty() {
            let mut errors: Vec<(String, usize)> = errors.into_iter().collect();
            errors.sort_by_key(|x| -(x.1 as i32));

            warn!("Errors constructing Rules: {:#?}", &errors);
        }

        Rules { rules }
    }
}

impl Tokenizer {
    pub fn from_xml<P: AsRef<std::path::Path>>(
        path: P,
        build_info: &mut BuildInfo,
        chunker: Option<chunk::Chunker>,
        options: TokenizerOptions,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        use log::warn;

        let rules = super::parse_structure::read_disambiguation_rules(path);
        let mut error = None;

        let rules: Vec<_> = rules
            .into_iter()
            .filter_map(|x| match x {
                Ok((rule_structure, group, _)) => {
                    let id = rule_structure.id.as_ref().map_or_else(
                        || {
                            let group = group.expect("must have group if ID not set");
                            format!("{}.{}", group.id, group.n)
                        },
                        |x| x.clone(),
                    );

                    match DisambiguationRule::from_rule_structure(rule_structure, build_info) {
                        Ok(mut rule) => {
                            if error.is_none()
                                && (options.ids.is_empty() || options.ids.contains(&id))
                                && !options.ignore_ids.contains(&id)
                            {
                                rule.id = id;

                                Some(rule)
                            } else {
                                None
                            }
                        }
                        Err(x) => {
                            error = Some(format!("[Rule] {}", x));
                            None
                        }
                    }
                }
                Err(x) => {
                    error = Some(format!("[Structure] {}", x));
                    None
                }
            })
            .collect();

        if let Some(x) = error {
            if options.allow_errors {
                warn!("Error constructing Disambiguator: {}", x)
            } else {
                return Err(format!("Error constructing Disambiguator: {}", x).into());
            }
        }

        Ok(Tokenizer {
            tagger: build_info.tagger().clone(),
            chunker,
            rules,
            options,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct ModelData {
    outcome_labels: Vec<String>,
    pmap: DefaultHashMap<String, chunk::Context>,
}

impl From<ModelData> for chunk::Model {
    fn from(data: ModelData) -> Self {
        chunk::Model {
            outcome_labels: data.outcome_labels,
            pmap: data
                .pmap
                .into_iter()
                .map(|(key, value)| (chunk::hash::hash_str(&key), value))
                .collect::<DefaultHashMap<_, _>>(),
        }
    }
}

impl chunk::Chunker {
    pub fn from_json<R: std::io::Read>(reader: R) -> chunk::Chunker {
        #[derive(Serialize, Deserialize)]
        struct ChunkData {
            token_model: ModelData,
            pos_model: ModelData,
            pos_tagdict: DefaultHashMap<String, Vec<String>>,
            chunk_model: ModelData,
        }

        let chunk_data: ChunkData = serde_json::from_reader(reader).unwrap();
        chunk::Chunker {
            token_model: chunk::MaxentTokenizer {
                model: chunk_data.token_model.into(),
            },
            pos_model: chunk::MaxentPosTagger {
                model: chunk_data.pos_model.into(),
                tagdict: chunk_data.pos_tagdict,
            },
            chunk_model: chunk::MaxentChunker {
                model: chunk_data.chunk_model.into(),
            },
        }
    }
}

impl POSFilter {
    pub fn new(matcher: PosMatcher) -> Self {
        POSFilter { matcher }
    }
}

mod composition {
    use super::*;
    use crate::{
        rule::engine::composition::{
            AndAtom, Atom, Composition, FalseAtom, NotAtom, OffsetAtom, OrAtom, Part, Quantifier,
            TrueAtom,
        },
        utils::regex::SerializeRegex,
    };

    impl Matcher {
        pub fn new_regex(regex: SerializeRegex, negate: bool, empty_always_false: bool) -> Self {
            Matcher {
                matcher: either::Right(regex),
                negate,
                case_sensitive: true, // handled by regex
                empty_always_false,
            }
        }

        pub fn new_string(
            string_or_idx: either::Either<String, usize>,
            negate: bool,
            case_sensitive: bool,
            empty_always_false: bool,
        ) -> Self {
            Matcher {
                matcher: either::Left(string_or_idx),
                negate,
                case_sensitive,
                empty_always_false,
            }
        }

        pub fn needs_graph(&self) -> bool {
            matches!(&self.matcher, either::Left(either::Right(_)))
        }
    }

    impl Quantifier {
        pub fn new(min: usize, max: usize) -> Self {
            assert!(max >= min);
            Quantifier { min, max }
        }
    }

    impl AndAtom {
        pub fn and(atoms: Vec<Atom>) -> Atom {
            let mut atoms: Vec<_> = atoms
                .into_iter()
                .filter(|x| !matches!(x, Atom::TrueAtom { .. }))
                .collect();

            if atoms.is_empty() {
                (TrueAtom {}).into()
            } else if atoms.len() == 1 {
                atoms.remove(0)
            } else {
                (AndAtom { atoms }).into()
            }
        }
    }

    impl OrAtom {
        pub fn or(atoms: Vec<Atom>) -> Atom {
            let mut atoms: Vec<_> = atoms
                .into_iter()
                .filter(|x| !matches!(x, Atom::FalseAtom { .. }))
                .collect();

            if atoms.is_empty() {
                (FalseAtom {}).into()
            } else if atoms.len() == 1 {
                atoms.remove(0)
            } else {
                (OrAtom { atoms }).into()
            }
        }
    }

    impl NotAtom {
        pub fn not(atom: Atom) -> Atom {
            match atom {
                Atom::TrueAtom { .. } => FalseAtom::default().into(),
                Atom::FalseAtom { .. } => TrueAtom::default().into(),
                x => (NotAtom { atom: Box::new(x) }).into(),
            }
        }
    }

    impl OffsetAtom {
        pub fn new(atom: Atom, offset: isize) -> Self {
            OffsetAtom {
                atom: Box::new(atom),
                offset,
            }
        }
    }

    impl Composition {
        pub fn new(parts: Vec<Part>) -> Self {
            let mut group_ids_to_idx = DefaultHashMap::default();
            group_ids_to_idx.insert(0, 0);
            let mut current_id = 1;

            for (i, part) in parts.iter().enumerate() {
                if part.visible {
                    group_ids_to_idx.insert(current_id, i + 1);
                    current_id += 1;
                }
            }

            let can_stop_mask = (0..parts.len())
                .map(|i| parts[i..].iter().all(|x| x.quantifier.min == 0))
                .collect();

            Composition {
                parts,
                group_ids_to_idx,
                can_stop_mask,
            }
        }
    }
}
