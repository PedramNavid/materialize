// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

#[cfg(test)]
mod tests {
    use lowertest::*;

    use std::collections::HashMap;

    use lazy_static::lazy_static;
    use ore::result::ResultExt;
    use proc_macro2::TokenTree;
    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    #[derive(Debug, Deserialize, PartialEq, Serialize, MzReflect)]
    struct ZeroArg;

    #[derive(Debug, Deserialize, PartialEq, Serialize, MzReflect)]
    struct SingleUnnamedArg(Box<f64>);

    #[derive(Debug, Deserialize, PartialEq, Serialize, MzReflect)]
    struct OptionalArg(bool, #[serde(default)] (f64, u32));

    #[derive(Debug, Deserialize, PartialEq, Serialize, MzReflect)]
    struct MultiUnnamedArg(Vec<(usize, Vec<(String, usize)>, usize)>, String);

    #[derive(Debug, Deserialize, PartialEq, Serialize, MzReflect)]
    struct MultiNamedArg {
        fizz: Vec<Option<bool>>,
        #[serde(default)]
        bizz: Vec<Vec<(SingleUnnamedArg, bool)>>,
    }

    #[derive(Debug, Deserialize, PartialEq, Serialize, MzReflect)]
    struct FirstArgEnum {
        test_enum: Box<TestEnum>,
        #[serde(default)]
        second_arg: String,
    }

    #[derive(Debug, Deserialize, PartialEq, Serialize, MzReflect)]
    enum TestEnum {
        SingleNamedField {
            foo: Vec<usize>,
        },
        MultiNamedFields {
            #[serde(default)]
            bar: Option<String>,
            #[serde(default)]
            baz: bool,
        },
        SingleUnnamedField(SingleUnnamedArg),
        SingleUnnamedField2(Vec<i64>),
        SingleUnnamedField3(MultiNamedArg),
        SingleUnnamedZeroArgField(ZeroArg),
        MultiUnnamedFields(MultiUnnamedArg, MultiNamedArg, Box<TestEnum>),
        MultiUnnamedFields2(OptionalArg, FirstArgEnum, #[serde(default)] String),
        MultiUnnamedZeroArgFields(ZeroArg, ZeroArg),
        MultiUnnamedFieldsFirstZeroArg(ZeroArg, OptionalArg),
        Unit,
    }

    lazy_static! {
        static ref RTI: ReflectedTypeInfo = {
            let mut rti = ReflectedTypeInfo::default();
            TestEnum::add_to_reflected_type_info(&mut rti);
            rti
        };
    }

    #[derive(Default)]
    struct TestOverrideDeserializeContext;

    impl TestDeserializeContext for TestOverrideDeserializeContext {
        /// This increments all numbers of type "usize" by one.
        /// If a positive f64 has been specified with +<the number>,
        /// ignore the +.
        /// Define an alternate syntax for `MultiUnnamedArg`:
        /// * (<usize1> "string") creates
        ///   `MultiUnnamedArg([(<usize1>, [("string", <usize1>)], <usize1>)], "string")`
        /// * "string" creates `MultiUnnamedArg([], "string")`
        /// * "<usize1>" creates
        ///   `MultiUnnamedArg([(<usize1>, [("", <usize1>)], <usize1>)], "")
        fn override_syntax<I>(
            &mut self,
            first_arg: TokenTree,
            rest_of_stream: &mut I,
            type_name: &str,
            rti: &ReflectedTypeInfo,
        ) -> Result<Option<String>, String>
        where
            I: Iterator<Item = TokenTree>,
        {
            if type_name == "MultiUnnamedArg" {
                if let TokenTree::Literal(literal) = first_arg.clone() {
                    let litval = literal.to_string();
                    if litval.starts_with('"') {
                        return Ok(Some(format!("[[], {}]", litval)));
                    } else {
                        let usize_lit = litval.parse::<usize>().map_err(|_| {
                            format!("{} cannot be parsed as a usize or a string", litval)
                        })?;
                        let mut stream_peek = rest_of_stream.peekable();
                        let str_lit = if stream_peek.peek().is_some() {
                            match stream_peek.next() {
                                Some(TokenTree::Literal(literal)) => literal.to_string(),
                                unexpected => {
                                    return Err(format!(
                                        "unexpected second argument for MultiUnnamedArg {:?}",
                                        unexpected
                                    ))
                                }
                            }
                        } else {
                            "".to_string()
                        };
                        return Ok(Some(
                            serde_json::to_string(&MultiUnnamedArg(
                                vec![(usize_lit, vec![(str_lit.clone(), usize_lit)], usize_lit)],
                                str_lit,
                            ))
                            .unwrap(),
                        ));
                    }
                }
            } else if type_name == "f64" {
                if let TokenTree::Punct(punct) = first_arg.clone() {
                    if punct.as_char() == '+' {
                        return to_json(rest_of_stream, type_name, rti, self);
                    }
                }
            } else if type_name == "usize" {
                if let TokenTree::Literal(literal) = first_arg {
                    let litval = literal.to_string().parse::<usize>().map_err_to_string()?;
                    return Ok(Some(format!("{}", litval + 1)));
                }
            }
            Ok(None)
        }

        /// This decrements all numbers of type "usize" by one.
        fn reverse_syntax_override(
            &mut self,
            json: &Value,
            type_name: &str,
            _rti: &ReflectedTypeInfo,
        ) -> Option<String> {
            if type_name == "usize" {
                let result: usize = json.as_u64().unwrap() as usize;
                if result == 0 {
                    return Some(0.to_string());
                } else {
                    return Some((result - 1).to_string());
                }
            }
            None
        }
    }

    fn create_test_enum(
        s: &str,
        args: &HashMap<String, Vec<String>>,
    ) -> Result<Option<TestEnum>, String> {
        let stream = tokenize(s)?;
        if args.get("override").is_some() {
            deserialize_optional(
                &mut stream.into_iter(),
                "TestEnum",
                &RTI,
                &mut TestOverrideDeserializeContext::default(),
            )
        } else {
            deserialize_optional(
                &mut stream.into_iter(),
                "TestEnum",
                &RTI,
                &mut GenericTestDeserializeContext::default(),
            )
        }
    }

    fn build(s: &str, args: &HashMap<String, Vec<String>>) -> Result<String, String> {
        // 1) Go from original spec to TestEnum.
        let result: Option<TestEnum> = create_test_enum(s, args)?;
        // 2) Go from TestEnum back to a new spec.
        let (json, new_s) = if let Some(result) = &result {
            let json = serde_json::to_value(result).map_err_to_string()?;
            let new_s = if args.get("override").is_some() {
                from_json(
                    &json,
                    "TestEnum",
                    &RTI,
                    &mut TestOverrideDeserializeContext::default(),
                )
            } else {
                from_json(
                    &json,
                    "TestEnum",
                    &RTI,
                    &mut GenericTestDeserializeContext::default(),
                )
            };
            (json, new_s)
        } else {
            (serde_json::json!(null), "".to_string())
        };
        // 3) The two specs cannot be directly compared to see if the roundtrip
        //    is successful because of the syntax supports multiple ways of
        //    specifying the same thing. So convert the new spec back into a
        //    TestEnum and compare the TestEnums.
        let new_result = create_test_enum(&new_s, &args)?;
        if !new_result.eq(&result) {
            return Err(format!(
                "Round trip failed. New spec:\n{}
                Original TestEnum\n{:?}
                New TestEnum:\n{:?}
                JSON for original TestEnum:\n{}",
                new_s, result, new_result, json
            ));
        }
        Ok(format!("{:?}", result))
    }

    #[test]
    fn run() {
        datadriven::walk("tests/testdata", |f| {
            f.run(move |s| -> String {
                match s.directive.as_str() {
                    "build" => match build(&s.input, &s.args) {
                        Ok(msg) => format!("{}\n", msg.trim_end()),
                        Err(err) => format!("error: {}\n", err),
                    },
                    _ => panic!("unknown directive: {}", s.directive),
                }
            })
        });
    }
}
