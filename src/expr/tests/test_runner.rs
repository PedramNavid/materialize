// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

mod test {
    use expr::canonicalize::{canonicalize_equivalences, canonicalize_predicates};
    use expr::{MapFilterProject, MirScalarExpr};
    use expr_test_util::*;
    use lowertest::{deserialize, tokenize};
    use ore::str::separated;
    use repr::RelationType;

    fn reduce(s: &str) -> Result<MirScalarExpr, String> {
        let mut input_stream = tokenize(&s)?.into_iter();
        let mut ctx = MirScalarExprDeserializeContext::default();
        let mut scalar: MirScalarExpr =
            deserialize(&mut input_stream, "MirScalarExpr", &RTI, &mut ctx)?;
        let typ: RelationType = deserialize(&mut input_stream, "RelationType", &RTI, &mut ctx)?;
        let before = scalar.typ(&typ);
        scalar.reduce(&typ);
        let after = scalar.typ(&typ);
        // Verify that `reduce` did not change the type of the scalar.
        if before.scalar_type != after.scalar_type {
            return Err(format!(
                "FAIL: Type of scalar has changed:\nbefore: {:?}\nafter: {:?}\n",
                before, after
            ));
        }
        Ok(scalar)
    }

    fn test_canonicalize_pred(s: &str) -> Result<Vec<MirScalarExpr>, String> {
        let mut input_stream = tokenize(&s)?.into_iter();
        let mut ctx = MirScalarExprDeserializeContext::default();
        let input_predicates: Vec<MirScalarExpr> =
            deserialize(&mut input_stream, "Vec<MirScalarExpr>", &RTI, &mut ctx)?;
        let typ: RelationType = deserialize(&mut input_stream, "RelationType", &RTI, &mut ctx)?;
        // predicate canonicalization is meant to produce the same output regardless of the
        // order of the input predicates.
        let mut predicates1 = input_predicates.clone();
        canonicalize_predicates(&mut predicates1, &typ);
        let mut predicates2 = input_predicates.clone();
        predicates2.sort();
        canonicalize_predicates(&mut predicates2, &typ);
        let mut predicates3 = input_predicates;
        predicates3.sort();
        predicates3.reverse();
        canonicalize_predicates(&mut predicates3, &typ);
        if predicates1 != predicates2 || predicates1 != predicates3 {
            Err(format!(
                "predicate canonicalization resulted in unrealiable output: [{}] vs [{}] vs [{}]",
                separated(", ", predicates1.iter().map(|p| p.to_string())),
                separated(", ", predicates2.iter().map(|p| p.to_string())),
                separated(", ", predicates3.iter().map(|p| p.to_string())),
            ))
        } else {
            Ok(predicates1)
        }
    }

    /// Builds a [MapFilterProject] of a certain arity, then modifies it.
    /// The test syntax is `<input_arity> [<commands>]`
    /// The syntax for a command is `<name_of_command> [<args>]`
    fn test_mfp(s: &str) -> Result<MapFilterProject, String> {
        let mut input_stream = tokenize(&s)?.into_iter();
        let mut ctx = MirScalarExprDeserializeContext::default();
        let input_arity: usize = deserialize(&mut input_stream, "usize", &RTI, &mut ctx)?;
        let mut mfp = MapFilterProject::new(input_arity);
        while let Some(proc_macro2::TokenTree::Ident(ident)) = input_stream.next() {
            match &ident.to_string()[..] {
                "map" => {
                    let map: Vec<MirScalarExpr> =
                        deserialize(&mut input_stream, "Vec<MirScalarExpr>", &RTI, &mut ctx)?;
                    mfp = mfp.map(map);
                }
                "filter" => {
                    let filter: Vec<MirScalarExpr> =
                        deserialize(&mut input_stream, "Vec<MirScalarExpr>", &RTI, &mut ctx)?;
                    mfp = mfp.filter(filter);
                }
                "project" => {
                    let project: Vec<usize> =
                        deserialize(&mut input_stream, "Vec<usize>", &RTI, &mut ctx)?;
                    mfp = mfp.project(project);
                }
                "optimize" => mfp.optimize(),
                unsupported => {
                    return Err(format!("Unsupport MFP command {}", unsupported));
                }
            }
        }
        Ok(mfp)
    }

    fn test_canonicalize_equiv(s: &str) -> Result<Vec<Vec<MirScalarExpr>>, String> {
        let mut input_stream = tokenize(&s)?.into_iter();
        let mut ctx = MirScalarExprDeserializeContext::default();
        let mut equivalences: Vec<Vec<MirScalarExpr>> =
            deserialize(&mut input_stream, "Vec<Vec<MirScalarExpr>>", &RTI, &mut ctx)?;
        let input_type: RelationType =
            deserialize(&mut input_stream, "RelationType", &RTI, &mut ctx)?;
        canonicalize_equivalences(&mut equivalences, &[input_type]);
        Ok(equivalences)
    }

    #[test]
    fn run() {
        datadriven::walk("tests/testdata", |f| {
            f.run(move |s| -> String {
                match s.directive.as_str() {
                    // tests simplification of scalars
                    "reduce" => match reduce(&s.input) {
                        Ok(scalar) => {
                            format!("{}\n", scalar)
                        }
                        Err(err) => format!("error: {}\n", err),
                    },
                    "canonicalize" => match test_canonicalize_pred(&s.input) {
                        Ok(preds) => {
                            format!("{}\n", separated("\n", preds.iter().map(|p| p.to_string())))
                        }
                        Err(err) => format!("error: {}\n", err),
                    },
                    "mfp" => match test_mfp(&s.input) {
                        Ok(mfp) => {
                            let (map, filter, project) = mfp.as_map_filter_project();
                            format!(
                                "[{}]\n[{}]\n[{}]\n",
                                separated(" ", map.iter()),
                                separated(" ", filter.iter()),
                                separated(" ", project.iter())
                            )
                        }
                        Err(err) => format!("error: {}\n", err),
                    },
                    "canonicalize-join" => match test_canonicalize_equiv(&s.input) {
                        Ok(equivalences) => {
                            format!(
                                "{}\n",
                                separated(
                                    "\n",
                                    equivalences.iter().map(|e| format!(
                                        "[{}]",
                                        separated(" ", e.iter().map(|expr| format!("{}", expr)))
                                    ))
                                )
                            )
                        }
                        Err(err) => format!("error: {}\n", err),
                    },
                    _ => panic!("unknown directive: {}", s.directive),
                }
            })
        });
    }
}
