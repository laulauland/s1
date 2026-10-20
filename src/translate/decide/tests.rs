use super::*;
use proptest::prelude::*;

const S1: ProtocolId = ProtocolId::Systemone;
const OA: ProtocolId = ProtocolId::OpenaiDecisions;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn to_s1(v: &Value) -> Result<Translated, S1Error> {
    Decide.request(OA, S1, &serde_json::to_vec(v).unwrap())
}

fn to_oa(v: &Value) -> Result<Translated, S1Error> {
    Decide.request(S1, OA, &serde_json::to_vec(v).unwrap())
}

fn body(t: &Translated) -> Value {
    serde_json::from_slice(&t.body).unwrap()
}

fn resp(from: ProtocolId, to: ProtocolId, v: &Value, map: &NameMap) -> Value {
    let out = Decide
        .response(from, to, &serde_json::to_vec(v).unwrap(), map)
        .unwrap();
    serde_json::from_slice(&out).unwrap()
}

/// Deterministic systemone answers for a systemone request, keyed in
/// reverse order (so tests see that order comes from the request).
fn fake_systemone_answers(req: &Value) -> Value {
    let mut answers: Vec<(String, Value)> = Vec::new();
    for (name, q) in req["questions"].as_object().unwrap() {
        let a = match q["type"].as_str().unwrap() {
            "noul" => json!({"type": "noul", "noul": 0.25}),
            "choice" => {
                let c = q["criteria"].as_object().unwrap();
                let first = c.keys().next().unwrap().clone();
                let p: Map<String, Value> = c.keys().map(|k| (k.clone(), json!(0.5))).collect();
                json!({"type": "choice", "choice": first, "confidence": 0.4, "probabilities": p})
            }
            "score" => {
                let levels = q["criteria"].as_array().unwrap();
                let legend: Map<String, Value> = levels
                    .iter()
                    .enumerate()
                    .map(|(i, l)| (i.to_string(), l.clone()))
                    .collect();
                let p: Map<String, Value> = (0..levels.len())
                    .map(|i| (i.to_string(), json!(0.1)))
                    .collect();
                json!({"type": "score", "score": 1.5, "confidence": 0.3, "legend": legend, "probabilities": p})
            }
            other => panic!("{other}"),
        };
        answers.push((name.clone(), a));
    }
    answers.reverse();
    let answers: Map<String, Value> = answers.into_iter().collect();
    json!({"model": "jev-1.13.0", "answers": answers, "usage": {"input_tokens": 10, "output_tokens": 2}})
}

/// Deterministic OpenAI answers for an OpenAI request.
fn fake_openai_answers(req: &Value) -> Value {
    let answers: Vec<Value> = req["questions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| {
            let name = q.get("name").cloned().unwrap_or(Value::Null);
            match q["type"].as_str().unwrap() {
                "predicate" => json!({"type": "predicate", "name": name, "probability": 0.75}),
                "choice" => {
                    let c = q["choices"].as_array().unwrap();
                    let p: Vec<Value> = c
                        .iter()
                        .map(|c| json!({"value": c["value"], "probability": 0.5}))
                        .collect();
                    json!({"type": "choice", "name": name, "choice": c[0]["value"], "probabilities": p, "confidence": 0.2})
                }
                "score" => {
                    let l = q["levels"].as_array().unwrap();
                    let p: Vec<Value> = l
                        .iter()
                        .enumerate()
                        .map(|(i, l)| json!({"value": i, "label": l["label"], "probability": 0.1}))
                        .collect();
                    json!({"type": "score", "name": name, "score": 0.5, "probabilities": p, "confidence": 0.9})
                }
                other => panic!("{other}"),
            }
        })
        .collect();
    json!({"model": "gpt-6-luna", "answers": answers, "usage": {"input_tokens": 7, "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0}, "output_tokens": 0, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 7}})
}

#[test]
fn recorded_openai_request_becomes_systemone() {
    let req: Value = serde_json::from_slice(&fixture("openai/triage.request.json")).unwrap();
    let t = to_s1(&req).unwrap();
    assert_eq!(
        body(&t),
        json!({
            "model": "gpt-6-luna",
            "state": "I was charged twice for my order.",
            "questions": {
                "billing": {"type": "noul", "instructions": "Is this message about billing?"},
                "q1": {"type": "choice", "instructions": "Which department?", "criteria": {"billing": "Payments", "technical": null, "bool:true": null}},
                "urgency": {"type": "score", "instructions": "How urgent?", "criteria": ["Low", "Medium: this week", "High"]}
            }
        })
    );
    assert_eq!(t.map.order, ["billing", "q1", "urgency"]);
    assert_eq!(t.map.client("q1"), None);
    assert_eq!(t.map.client("urgency"), Some("urgency"));
    assert_eq!(
        t.map.levels["urgency"],
        vec![json!("Low"), json!("Medium"), json!("High")]
    );
}

#[test]
fn recorded_openai_answers_become_systemone_answers() {
    // A systemone client asked an OpenAI backend the triage questions.
    let s1_req = json!({
        "model": "openai/gpt-6-luna",
        "state": "I was charged twice for my order.",
        "questions": {
            "billing": {"type": "noul", "instructions": "Is this message about billing?"},
            "dept": {"type": "choice", "instructions": "Which department?", "criteria": {"billing": "Payments", "technical": null, "bool:true": null}},
            "urgency": {"type": "score", "instructions": "How urgent?", "criteria": ["Low", "Medium", "High"]}
        }
    });
    let t = to_oa(&s1_req).unwrap();
    let oa = body(&t);
    assert_eq!(oa["questions"][1]["choices"][2], json!({"value": true}));
    assert_eq!(
        oa["questions"][1]["choices"][0],
        json!({"value": "billing", "description": "Payments"})
    );
    let recorded: Value = serde_json::from_slice(&fixture("openai/triage.response.json")).unwrap();
    let out = resp(OA, S1, &recorded, &t.map);
    assert_eq!(out["model"], "gpt-6-luna");
    assert_eq!(
        out["answers"]["billing"],
        json!({"type": "noul", "noul": 1.0})
    );
    assert_eq!(out["answers"]["dept"]["choice"], "billing");
    assert_eq!(
        out["answers"]["dept"]["probabilities"],
        json!({"billing": 1.0, "technical": 0.0, "bool:true": 0.0})
    );
    assert_eq!(out["answers"]["urgency"]["score"], 1.19);
    assert_eq!(
        out["answers"]["urgency"]["legend"],
        json!({"0": "Low", "1": "Medium", "2": "High"})
    );
    assert_eq!(out["answers"]["urgency"]["probabilities"]["1"], 0.73);
    assert_eq!(
        out["usage"],
        json!({"input_tokens": 375, "output_tokens": 0})
    );
    let keys: Vec<&String> = out["answers"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["billing", "dept", "urgency"]);
}

#[test]
fn refusal_becomes_a_per_question_error() {
    let req = json!({"model": "m", "state": "rate 7", "questions": {"q": {"type": "noul"}}});
    let t = to_oa(&req).unwrap();
    // No instructions: OpenAI gets an empty string (it refuses, see the fixture).
    assert_eq!(body(&t)["questions"][0]["instructions"], "");
    let recorded: Value = serde_json::from_slice(&fixture("openai/refusal.response.json")).unwrap();
    let out = resp(OA, S1, &recorded, &t.map);
    assert_eq!(out["answers"]["q"], refusal_answer());
    assert_eq!(out["answers"]["q"]["error"]["code"], "refusal");
    assert!(
        out["answers"]["q"].get("noul").is_none(),
        "no invented value"
    );
}

#[test]
fn systemone_answers_become_openai_answers_in_request_order() {
    let req: Value = serde_json::from_slice(&fixture("openai/triage.request.json")).unwrap();
    let t = to_s1(&req).unwrap();
    let s1_resp = fake_systemone_answers(&body(&t));
    let out = resp(S1, OA, &s1_resp, &t.map);
    assert_eq!(out["model"], "jev-1.13.0");
    let a = out["answers"].as_array().unwrap();
    assert_eq!(a.len(), 3);
    assert_eq!(
        a[0],
        json!({"type": "predicate", "name": "billing", "probability": 0.25})
    );
    assert_eq!(a[1]["name"], Value::Null);
    assert_eq!(a[1]["choice"], "billing");
    assert_eq!(
        a[1]["probabilities"][2],
        json!({"value": true, "probability": 0.5})
    );
    assert_eq!(a[2]["type"], "score");
    // Labels come back as the client wrote them, not "Medium: this week".
    assert_eq!(
        a[2]["probabilities"][1],
        json!({"value": 1, "label": "Medium", "probability": 0.1})
    );
    assert_eq!(out["usage"]["total_tokens"], 12);
    assert_eq!(out["usage"]["input_tokens_details"]["cached_tokens"], 0);
}

#[test]
fn systemone_error_answers_become_refusals() {
    let req = json!({"model": "m", "input": "x", "questions": [{"type": "predicate", "instructions": "a?"}, {"type": "predicate", "instructions": "b?"}]});
    let t = to_s1(&req).unwrap();
    let s1_resp = json!({"model": "m", "answers": {"q0": {"type": "error", "error": {"code": "x"}}}, "usage": {}});
    let out = resp(S1, OA, &s1_resp, &t.map);
    assert_eq!(out["answers"][0], json!({"type": "refusal", "name": null}));
    assert_eq!(out["answers"][1], json!({"type": "refusal", "name": null}));
}

#[test]
fn unnamed_questions_get_free_q_names() {
    let req = json!({"model": "m", "input": "x", "questions": [
        {"type": "predicate", "instructions": "a"},
        {"type": "predicate", "instructions": "b", "name": "q0"},
        {"type": "predicate", "instructions": "c", "name": "dup"},
        {"type": "predicate", "instructions": "d", "name": "dup"},
        {"type": "predicate", "instructions": "e"}
    ]});
    let t = to_s1(&req).unwrap();
    assert_eq!(t.map.order, ["q0_1", "q0", "dup", "dup_1", "q4"]);
    let keys: Vec<String> = body(&t)["questions"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(keys, t.map.order);
    assert_eq!(t.map.client("dup_1"), Some("dup"));
    assert_eq!(t.map.client("q0_1"), None);
}

#[test]
fn noul_criteria_are_appended_and_read_back() {
    let req = json!({"model": "m", "state": "s", "questions": {"ok": {
        "type": "noul", "instructions": "Is it fine?",
        "criteria": {"true": "It works.", "false": "It is broken."}
    }}});
    let t = to_oa(&req).unwrap();
    assert_eq!(
        body(&t)["questions"][0]["instructions"],
        "Is it fine?\n\nTrue if: It works.\n\nFalse if: It is broken."
    );
    assert_eq!(
        body(&to_s1(&body(&t)).unwrap())["questions"],
        req["questions"]
    );
    assert_eq!(
        split_criteria("False if: no"),
        (String::new(), None, Some("no".into()))
    );
    assert_eq!(split_criteria("plain"), ("plain".into(), None, None));
}

#[test]
fn score_levels_over_ten_are_unsupported() {
    let levels: Vec<Value> = (0..11).map(|i| json!({"label": i.to_string()})).collect();
    let req = json!({"model": "m", "input": "x", "questions": [{"type": "score", "instructions": "n?", "levels": levels}]});
    let e = to_s1(&req).unwrap_err();
    assert_eq!(e.code(), "unsupported", "{e}");
}

#[test]
fn box_is_unsupported_toward_openai() {
    let req = json!({"model": "m", "state": "photo", "questions": {"where": {"type": "box", "instructions": "Find it."}}});
    assert_eq!(to_oa(&req).unwrap_err().code(), "unsupported");
}

#[test]
fn bool_like_string_choice_is_unsupported() {
    let req = json!({"model": "m", "input": "x", "questions": [{"type": "choice", "instructions": "?", "choices": [{"value": "bool:true"}, {"value": true}]}]});
    assert_eq!(to_s1(&req).unwrap_err().code(), "unsupported");
    let dup = json!({"model": "m", "input": "x", "questions": [{"type": "choice", "instructions": "?", "choices": [{"value": "a"}, {"value": "a"}]}]});
    assert_eq!(to_s1(&dup).unwrap_err().code(), "invalid_request");
}

#[test]
fn recorded_typed_choice_answer_keeps_bool_and_string_apart() {
    let req = json!({"model": "m", "state": "x", "questions": {"t": {"type": "choice", "instructions": "x", "criteria": {"true": null, "bool:true": null}}}});
    let t = to_oa(&req).unwrap();
    assert_eq!(
        body(&t)["questions"][0]["choices"],
        json!([{"value": "true"}, {"value": true}])
    );
    let recorded: Value =
        serde_json::from_slice(&fixture("openai/typed_choice.response.json")).unwrap();
    let out = resp(OA, S1, &recorded, &t.map);
    assert_eq!(out["answers"]["t"]["choice"], "true");
    assert_eq!(
        out["answers"]["t"]["probabilities"],
        json!({"true": 0.52, "bool:true": 0.48})
    );
}

#[test]
fn image_data_urls_become_assets_and_back() {
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk";
    let req = json!({"model": "m", "input": [{"role": "user", "content": [
        {"type": "input_text", "text": "Inspect the product in this photo."},
        {"type": "input_image", "image_url": format!("data:image/png;base64,{png}"), "detail": "low"}
    ]}], "questions": [{"type": "predicate", "name": "damaged", "instructions": "Damaged?"}],
    "prompt_cache_key": "k", "safety_identifier": "u"});
    let t = to_s1(&req).unwrap();
    let s1 = body(&t);
    assert_eq!(
        s1["state"],
        "Inspect the product in this photo.<s1-image-0>"
    );
    assert_eq!(
        s1["assets"]["<s1-image-0>"],
        json!({"type": "image", "format": "png", "data_base64": png})
    );
    assert!(s1.get("prompt_cache_key").is_none());
    // And back toward OpenAI: the placeholder splits the text.
    let oa = body(&to_oa(&s1).unwrap());
    assert_eq!(
        oa["input"],
        json!([{"role": "user", "content": [
            {"type": "input_text", "text": "Inspect the product in this photo."},
            {"type": "input_image", "image_url": format!("data:image/png;base64,{png}")}
        ]}])
    );
}

#[test]
fn image_urls_must_be_fetched_first_and_formats_are_checked() {
    let with = |url: &str| {
        json!({"model": "m", "input": [{"role": "user", "content": [
            {"type": "input_image", "image_url": url}
        ]}], "questions": [{"type": "predicate", "instructions": "?"}]})
    };
    let e = to_s1(&with("https://example.com/a.png")).unwrap_err();
    assert_eq!(e.code(), "invalid_request");
    assert!(e.to_string().contains("not fetched"), "{e}");
    let e = to_s1(&with("data:image/webp;base64,AAAA")).unwrap_err();
    assert_eq!(e.code(), "unsupported");
    let roles = json!({"model": "m", "input": [{"role": "developer", "content": "be brief"}], "questions": []});
    assert_eq!(to_s1(&roles).unwrap_err().code(), "unsupported");
}

#[test]
fn placeholders_outside_state_and_clef_images() {
    let req = json!({"model": "m", "state": "look", "questions": {"q": {"type": "noul", "instructions": "Is p1 red?"}},
        "assets": {"p1": {"type": "image", "format": "jpg", "data_base64": "/9j/AAAA"}}});
    assert_eq!(to_oa(&req).unwrap_err().code(), "unsupported");
    let clef = json!({"model": "m", "state": "look", "questions": {"q": {"type": "noul"}},
        "images": ["data:image/jpeg;base64,/9j/AAAA", {"content_type": "image/png", "base64": "iVBO"}]});
    let oa = body(&to_oa(&clef).unwrap());
    let parts = oa["input"][0]["content"].as_array().unwrap();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0], json!({"type": "input_text", "text": "look"}));
    assert_eq!(parts[2]["image_url"], "data:image/png;base64,iVBO");
}

#[test]
fn client_text_that_looks_like_criteria_reads_back_unchanged() {
    let round = |q: Value| {
        let req = json!({"model": "m", "state": "s", "questions": {"a": q}});
        let back = body(&to_s1(&body(&to_oa(&req).unwrap())).unwrap());
        assert_eq!(back["questions"], req["questions"]);
    };
    round(json!({"type": "noul", "instructions": "Check.\n\nTrue if: it is red"}));
    round(json!({"type": "noul", "instructions": "True if: red"}));
    round(json!({"type": "noul", "instructions": "False if: red"}));
    round(
        json!({"type": "noul", "instructions": "Q", "criteria": {"true": "red\n\nFalse if: blue"}}),
    );
    round(
        json!({"type": "noul", "instructions": "Q\n\nFalse if: x", "criteria": {"false": "True if: y"}}),
    );
    // Text that already has escape marks keeps them.
    round(
        json!({"type": "noul", "instructions": "True if\u{200B}: a\n\nFalse if\u{200B}\u{200B}: b"}),
    );
    round(json!({"type": "noul", "instructions": "\u{200B}True if \u{200B}: x"}));
    // What OpenAI sees: the client's own marker text is escaped, s1's is not.
    let req = json!({"model": "m", "state": "s", "questions": {"a": {"type": "noul",
        "instructions": "Check.\n\nTrue if: it is red", "criteria": {"false": "blue"}}}});
    assert_eq!(
        body(&to_oa(&req).unwrap())["questions"][0]["instructions"],
        "Check.\n\nTrue if\u{200B}: it is red\n\nFalse if: blue"
    );
    assert_eq!(escape_criteria("no markers"), "no markers");
    assert_eq!(unescape_criteria("True if: x"), "True if: x");
}

#[test]
fn non_string_texts_go_to_openai_as_json_text() {
    let req = json!({"model": "m", "state": {"body": "hi"}, "questions": {
        "n": {"type": "noul", "instructions": {"ask": "red?"}, "criteria": {"true": ["a", 1]}},
        "c": {"type": "choice", "criteria": {"a": {"k": 1}, "b": null}},
        "s": {"type": "score", "criteria": [{"lvl": "low"}, "high"]}
    }});
    let oa = body(&to_oa(&req).unwrap());
    assert_eq!(oa["input"], r#"{"body":"hi"}"#);
    let back = body(&to_s1(&oa).unwrap());
    assert_eq!(back, json_texts(&req));
    assert_eq!(back["state"], r#"{"body":"hi"}"#);
    assert_eq!(back["questions"]["c"]["criteria"]["a"], r#"{"k":1}"#);
}

#[test]
fn image_round_trip_keeps_text_and_shares_identical_images() {
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk";
    let asset = json!({"type": "image", "format": "png", "data_base64": png});
    let req = json!({"model": "m", "state": "look at IMG now", "questions": {"q": {"type": "noul"}},
        "assets": {"IMG": asset}});
    let back = body(&to_s1(&body(&to_oa(&req).unwrap())).unwrap());
    // Placeholder names are s1's own; the text around them is unchanged.
    assert_eq!(back["state"], "look at <s1-image-0> now");
    assert_eq!(back["assets"], json!({"<s1-image-0>": asset}));
    // One asset used twice stays one asset used twice.
    let twice = json!({"model": "m", "state": "P\nP", "questions": {"q": {"type": "noul"}},
        "assets": {"P": asset}});
    let back = body(&to_s1(&body(&to_oa(&twice).unwrap())).unwrap());
    assert_eq!(back["state"], "<s1-image-0>\n<s1-image-0>");
    assert_eq!(back["assets"].as_object().unwrap().len(), 1);
    // Eleven images: no placeholder is a prefix of another.
    let parts: Vec<Value> = (0..11)
        .flat_map(|i| {
            [
                json!({"type": "input_image", "image_url": format!("data:image/png;base64,{png}{}", "A".repeat(i * 4))}),
                json!({"type": "input_text", "text": "0"}),
            ]
        })
        .collect();
    let oa = json!({"model": "m", "input": [{"role": "user", "content": parts}],
        "questions": [{"type": "predicate", "instructions": "?"}]});
    let s1 = body(&to_s1(&oa).unwrap());
    assert!(
        s1["state"]
            .as_str()
            .unwrap()
            .starts_with("<s1-image-0>0<s1-image-1>0")
    );
    let again = body(&to_oa(&s1).unwrap());
    assert_eq!(again["input"], oa["input"]);
}

#[test]
fn asset_in_state_and_a_question_is_unsupported_toward_openai() {
    let png = json!({"type": "image", "format": "png", "data_base64": "iVBORw0KGgo"});
    let req = json!({"model": "m", "state": "door", "questions": {"p": {"type": "noul",
        "instructions": "Is there a person in door?"}}, "assets": {"door": png}});
    let e = to_oa(&req).unwrap_err();
    assert_eq!(e.code(), "unsupported");
    assert!(e.to_string().contains("question `p`"), "{e}");
    let crit = json!({"model": "m", "state": "door", "questions": {"c": {"type": "choice",
        "criteria": {"yes": "a person in door", "no": null}}}, "assets": {"door": png}});
    assert_eq!(to_oa(&crit).unwrap_err().code(), "unsupported");
}

#[test]
fn systemone_bool_names_are_openai_booleans_both_ways() {
    // systemone names are strings; `bool:true` / `bool:false` are the
    // names of OpenAI's boolean values, in both directions.
    let req = json!({"model": "m", "state": "x", "questions": {"c": {"type": "choice", "instructions": "i",
        "criteria": {"bool:true": "d", "bool:false": null, "true": null}}}});
    let t = to_oa(&req).unwrap();
    assert_eq!(
        body(&t)["questions"][0]["choices"],
        json!([{"value": true, "description": "d"}, {"value": false}, {"value": "true"}])
    );
    let answer = json!({"model": "m", "answers": [{"type": "choice", "name": "c", "choice": true,
        "probabilities": [{"value": true, "probability": 0.6}, {"value": false, "probability": 0.1},
        {"value": "true", "probability": 0.3}], "confidence": 0.5}], "usage": {}});
    let out = resp(OA, S1, &answer, &t.map);
    assert_eq!(out["answers"]["c"]["choice"], "bool:true");
    assert_eq!(
        out["answers"]["c"]["probabilities"],
        json!({"bool:true": 0.6, "bool:false": 0.1, "true": 0.3})
    );
    assert_eq!(
        body(&to_s1(&body(&t)).unwrap())["questions"],
        req["questions"]
    );
}

#[test]
fn duplicate_openai_names_keep_every_answer() {
    let req = json!({"model": "m", "input": "x", "questions": [
        {"type": "predicate", "name": "a", "instructions": "1"},
        {"type": "predicate", "name": "a", "instructions": "2"},
        {"type": "predicate", "instructions": "3"}
    ]});
    let t = to_s1(&req).unwrap();
    assert_eq!(t.map.order, ["a", "a_1", "q2"]);
    let answers = json!({"model": "m", "answers": [
        {"type": "predicate", "name": "a", "probability": 0.1},
        {"type": "predicate", "name": "a", "probability": 0.2},
        {"type": "predicate", "name": null, "probability": 0.3}
    ], "usage": {}});
    let out = resp(OA, S1, &answers, &t.map);
    assert_eq!(
        out["answers"],
        json!({"a": {"type": "noul", "noul": 0.1}, "a_1": {"type": "noul", "noul": 0.2}, "q2": {"type": "noul", "noul": 0.3}})
    );
}

/// `req` with every non-string text (state, instructions, criteria values)
/// as its compact JSON text: what a round trip through OpenAI returns.
fn json_texts(req: &Value) -> Value {
    fn s(v: &mut Value) {
        if !v.is_string() && !v.is_null() {
            *v = Value::String(v.to_string());
        }
    }
    let mut out = req.clone();
    s(&mut out["state"]);
    for q in out["questions"].as_object_mut().unwrap().values_mut() {
        let q = q.as_object_mut().unwrap();
        if let Some(i) = q.get_mut("instructions") {
            s(i);
        }
        match q.get_mut("criteria") {
            Some(Value::Object(m)) => m.values_mut().for_each(s),
            Some(Value::Array(a)) => a.iter_mut().for_each(s),
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Properties (spec "Testing": Translation)
// ---------------------------------------------------------------------------

/// Plain text, or text built from pieces that look like appended noul
/// criteria and their escape marks.
fn words() -> BoxedStrategy<String> {
    let piece = prop_oneof![
        "[A-Za-z?.,' ]{1,12}",
        Just(TRUE_IF.to_string()),
        Just(FALSE_IF.to_string()),
        Just(format!("\n\n{TRUE_IF}")),
        Just(format!("\n\n{FALSE_IF}")),
        Just("\n\n".to_string()),
        Just("\u{200B}".to_string()),
        Just("True if\u{200B}: ".to_string()),
        Just("False if\u{200B}\u{200B}: ".to_string()),
    ];
    prop_oneof![
        "[A-Za-z?.,' ]{1,24}",
        Just(String::new()),
        proptest::collection::vec(piece, 1..5).prop_map(|p| p.concat()),
    ]
    .boxed()
}

/// A systemone text: a string, or (when `any`) also an object or a list,
/// as TypeSafe allows.
fn s1_text(any: bool) -> BoxedStrategy<Value> {
    if any {
        prop_oneof![
            words().prop_map(Value::String),
            words().prop_map(|w| json!({"k": w})),
            (words(), proptest::num::i32::ANY).prop_map(|(w, n)| json!([w, n])),
        ]
        .boxed()
    } else {
        words().prop_map(Value::String).boxed()
    }
}

fn choice_names() -> impl Strategy<Value = Vec<String>> {
    proptest::collection::vec(
        prop_oneof![
            "[a-z_]{1,8}",
            Just("true".to_string()),
            Just("false".to_string()),
            Just(BOOL_TRUE.to_string()),
            Just(BOOL_FALSE.to_string()),
        ],
        2..6,
    )
}

/// A systemone question without systemone-only features (`box`, images).
/// Texts are strings unless `any`.
fn s1_question(any: bool) -> impl Strategy<Value = Value> {
    // Empty instructions read back as absent ones: OpenAI always has
    // `instructions`, so the two cannot be told apart.
    let instructions =
        move || proptest::option::of(s1_text(any).prop_filter("non-empty", |v| v != &json!("")));
    prop_oneof![
        (
            instructions(),
            proptest::option::of((
                proptest::option::of(s1_text(any)),
                proptest::option::of(s1_text(any))
            )),
        )
            .prop_map(|(i, c)| {
                let mut q = json!({"type": "noul"});
                if let Some(i) = i {
                    q["instructions"] = i;
                }
                if let Some((t, f)) = c
                    && (t.is_some() || f.is_some())
                {
                    let mut m = Map::new();
                    if let Some(t) = t {
                        m.insert("true".into(), t);
                    }
                    if let Some(f) = f {
                        m.insert("false".into(), f);
                    }
                    q["criteria"] = Value::Object(m);
                }
                q
            }),
        (
            instructions(),
            choice_names(),
            proptest::collection::vec(proptest::option::of(s1_text(any)), 6),
        )
            .prop_map(|(i, names, descs)| {
                let mut c = Map::new();
                for (n, d) in names.into_iter().zip(descs) {
                    c.insert(n, d.unwrap_or(Value::Null));
                }
                let mut q = json!({"type": "choice", "criteria": c});
                if let Some(i) = i {
                    q["instructions"] = i;
                }
                q
            }),
        (
            instructions(),
            proptest::collection::vec(s1_text(any), 1..=MAX_SYSTEMONE_LEVELS)
        )
            .prop_map(|(i, levels)| {
                let mut q = json!({"type": "score", "criteria": levels});
                if let Some(i) = i {
                    q["instructions"] = i;
                }
                q
            }),
    ]
}

fn s1_request_with(any: bool) -> impl Strategy<Value = Value> {
    (
        s1_text(any),
        proptest::collection::vec(("[a-z_0-9]{1,8}", s1_question(any)), 1..6),
    )
        .prop_map(|(state, qs)| {
            let questions: Map<String, Value> = qs.into_iter().collect();
            json!({"model": "decide", "state": state, "questions": questions})
        })
}

/// A systemone request with string texts.
fn s1_request() -> impl Strategy<Value = Value> {
    s1_request_with(false)
}

/// An OpenAI input: one user message of text and image parts, never two
/// text parts in a row, at least one image; images come from a small set
/// so some repeat.
fn oa_image_input() -> impl Strategy<Value = Value> {
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk";
    let part = prop_oneof![
        "[A-Za-z?.,' 0-9<>-]{1,12}".prop_map(|t| json!({"type": "input_text", "text": t})),
        (0..3usize).prop_map(move |i| json!({"type": "input_image",
            "image_url": format!("data:image/png;base64,{png}{}", "A".repeat(i * 4))})),
    ];
    proptest::collection::vec(part, 1..8)
        .prop_filter("an image (text alone is a plain string)", |parts| {
            parts.iter().any(|p| p["type"] == "input_image")
        })
        .prop_map(|parts| {
            let mut out: Vec<Value> = Vec::new();
            for p in parts {
                let text = p["type"] == "input_text";
                if text && out.last().is_some_and(|l| l["type"] == "input_text") {
                    continue;
                }
                out.push(p);
            }
            json!([{"role": "user", "content": out}])
        })
}

fn oa_question() -> impl Strategy<Value = Value> {
    let name = proptest::option::of(prop_oneof![
        "[a-z]{1,4}",
        Just("q0".to_string()),
        Just("q1".to_string()),
        Just("dup".to_string()),
    ]);
    let kind = prop_oneof![
        Just(json!({"type": "predicate", "instructions": "yes?"})),
        choice_names().prop_map(|names| {
            let mut seen = HashSet::new();
            let choices: Vec<Value> = names
                .into_iter()
                .filter(|n| n != BOOL_TRUE && n != BOOL_FALSE && seen.insert(n.clone()))
                .map(|n| json!({"value": n}))
                .chain([json!({"value": true}), json!({"value": false})])
                .collect();
            json!({"type": "choice", "instructions": "which?", "choices": choices})
        }),
        proptest::collection::vec(words(), 2..=MAX_SYSTEMONE_LEVELS).prop_map(|l| {
            let levels: Vec<Value> = l.into_iter().map(|l| json!({"label": l})).collect();
            json!({"type": "score", "instructions": "how much?", "levels": levels})
        }),
    ];
    (name, kind).prop_map(|(n, mut q)| {
        if let Some(n) = n {
            q["name"] = json!(n);
        }
        q
    })
}

proptest! {
    /// systemone -> OpenAI Decisions -> systemone returns the same request.
    #[test]
    fn systemone_round_trip_is_identity(req in s1_request()) {
        let oa = to_oa(&req).unwrap();
        let back = to_s1(&body(&oa)).unwrap();
        prop_assert_eq!(body(&back), req);
    }

    /// With object and list texts too (TypeSafe allows them), the round
    /// trip returns each non-string text as its compact JSON text.
    #[test]
    fn systemone_round_trip_turns_non_string_texts_into_json_text(req in s1_request_with(true)) {
        let oa = to_oa(&req).unwrap();
        let back = to_s1(&body(&oa)).unwrap();
        prop_assert_eq!(body(&back), json_texts(&req));
    }

    /// OpenAI Decisions -> systemone -> OpenAI Decisions keeps image input
    /// exactly (text around images, order, and repeated images).
    #[test]
    fn openai_image_input_round_trip_is_identity(input in oa_image_input()) {
        let req = json!({"model": "m", "input": input,
            "questions": [{"type": "predicate", "name": "p", "instructions": "?"}]});
        let s1 = body(&to_s1(&req).unwrap());
        let back = body(&to_oa(&s1).unwrap());
        prop_assert_eq!(&back["input"], &req["input"]);
    }

    /// OpenAI questions (any names, duplicates too) through a systemone
    /// map: answers toward systemone keep one entry per question.
    #[test]
    fn every_openai_question_keeps_an_answer_toward_systemone(
        qs in proptest::collection::vec(oa_question(), 1..8),
    ) {
        let req = json!({"model": "decide", "input": "x", "questions": qs});
        let t = to_s1(&req).unwrap();
        let out = resp(OA, S1, &fake_openai_answers(&req), &t.map);
        prop_assert_eq!(out["answers"].as_object().unwrap().len(), qs.len());
    }

    /// OpenAI client, systemone backend: answers come back with the same
    /// names, the same types, and in request order.
    #[test]
    fn openai_client_answers_keep_names_and_order(
        qs in proptest::collection::vec(oa_question(), 1..8),
    ) {
        let req = json!({"model": "decide", "input": "x", "questions": qs});
        let t = to_s1(&req).unwrap();
        let s1_req = body(&t);
        prop_assert_eq!(s1_req["questions"].as_object().unwrap().len(), qs.len());
        let out = resp(S1, OA, &fake_systemone_answers(&s1_req), &t.map);
        let answers = out["answers"].as_array().unwrap();
        prop_assert_eq!(answers.len(), qs.len());
        for (q, a) in qs.iter().zip(answers) {
            prop_assert_eq!(&a["name"], q.get("name").unwrap_or(&Value::Null));
            prop_assert_eq!(&a["type"], &q["type"]);
        }
    }

    /// systemone client, OpenAI backend: answers keyed by the client's
    /// names, in request order.
    #[test]
    fn systemone_client_answers_keep_names_and_order(req in s1_request()) {
        let t = to_oa(&req).unwrap();
        let out = resp(OA, S1, &fake_openai_answers(&body(&t)), &t.map);
        let got: Vec<&String> = out["answers"].as_object().unwrap().keys().collect();
        let want: Vec<&String> = req["questions"].as_object().unwrap().keys().collect();
        prop_assert_eq!(got, want);
        for (name, q) in req["questions"].as_object().unwrap() {
            let ty = &out["answers"][name]["type"];
            prop_assert_eq!(ty, &q["type"]);
        }
    }

    /// Boolean and string choice values with the same text stay distinct
    /// after a round trip, in both directions.
    #[test]
    fn bool_and_string_choices_stay_distinct(flag in any::<bool>(), extra in "[a-z]{1,5}") {
        let text = flag.to_string();
        let req = json!({"model": "m", "input": "x", "questions": [{"type": "choice", "instructions": "?",
            "choices": [{"value": text}, {"value": flag}, {"value": extra}]}]});
        let t = to_s1(&req).unwrap();
        let s1_req = body(&t);
        let crit = s1_req["questions"]["q0"]["criteria"].as_object().unwrap();
        prop_assert_eq!(crit.len(), 3);
        let out = resp(S1, OA, &fake_systemone_answers(&s1_req), &t.map);
        let values: Vec<Value> = out["answers"][0]["probabilities"]
            .as_array().unwrap().iter().map(|p| p["value"].clone()).collect();
        prop_assert_eq!(values, vec![json!(text), json!(flag), json!(extra)]);
        // systemone client: names `true` and `bool:true` reach OpenAI as a
        // string and a boolean, and come back as the same two names.
        let back = to_s1(&body(&to_oa(&s1_req).unwrap())).unwrap();
        prop_assert_eq!(&body(&back)["questions"], &s1_req["questions"]);
    }
}

// Review B5: a generated placeholder must not occur in any text that
// systemone may expand, not only in the input text.
#[test]
fn placeholders_avoid_question_texts() {
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk";
    let req = json!({"model": "m", "input": [{"role": "user", "content": [
        {"type": "input_text", "text": "Look."},
        {"type": "input_image", "image_url": format!("data:image/png;base64,{png}")}
    ]}], "questions": [
        {"type": "predicate", "instructions": "Is <s1-image-0> red?"},
        {"type": "choice", "instructions": "i", "choices": [
            {"value": "a", "description": "like <s1-image-1-0>"}, {"value": "b"}]},
        {"type": "score", "instructions": "i", "levels": [
            {"label": "<s1-image-2-0>"}, {"label": "high"}]}
    ]});
    let s1 = body(&to_s1(&req).unwrap());
    let assets = s1["assets"].as_object().unwrap();
    assert_eq!(assets.len(), 1);
    let key = assets.keys().next().unwrap();
    let questions = serde_json::to_string(&s1["questions"]).unwrap();
    assert!(!questions.contains(key.as_str()), "{key} in {questions}");
    assert!(s1["state"].as_str().unwrap().contains(key.as_str()));
}

// Review B6: in a structured state only string values are expanded, and
// references are matched in the values, not in their escaped JSON text.
#[test]
fn structured_state_matches_asset_references_in_values_only() {
    let png = "iVBORw0KGgo";
    let asset = json!({"type": "image", "format": "png", "data_base64": png});
    let url = format!("data:image/png;base64,{png}");
    let req = json!({"model": "m", "state": {"IMG": "IMG"}, "questions": {"q": {"type": "noul"}},
        "assets": {"IMG": asset}});
    let oa = body(&to_oa(&req).unwrap());
    assert_eq!(
        oa["input"][0]["content"],
        json!([
            {"type": "input_text", "text": "{\"IMG\":\""},
            {"type": "input_image", "image_url": url},
            {"type": "input_text", "text": "\"}"}
        ])
    );
    // A reference with a quote and a newline still matches.
    let req = json!({"model": "m", "state": {"k": ["see a\"b\nc", 1]}, "questions": {"q": {"type": "noul"}},
        "assets": {"a\"b\nc": asset}});
    let oa = body(&to_oa(&req).unwrap());
    assert_eq!(
        oa["input"][0]["content"],
        json!([
            {"type": "input_text", "text": "{\"k\":[\"see "},
            {"type": "input_image", "image_url": url},
            {"type": "input_text", "text": "\",1]}"}
        ])
    );
    // A key that is only an object key is not a reference.
    let req = json!({"model": "m", "state": {"IMG": "x"}, "questions": {"q": {"type": "noul"}},
        "assets": {"IMG": asset}});
    assert_eq!(to_oa(&req).unwrap_err().code(), "unsupported");
}

// Review R2-4.1: one asset referenced many times must not expand without
// bound toward OpenAI; the budget is checked before each image is built.
#[test]
fn repeated_asset_references_have_an_expansion_budget() {
    let code = |r: Result<Translated, S1Error>| r.err().map(|e| e.code());
    let png = format!("iVBORw0KGgo{}", "A".repeat(1024 * 1024));
    let asset = json!({"type": "image", "format": "png", "data_base64": png});
    let reps = MAX_EXPANDED_IMAGES / png.len() + 1;
    // A string state.
    let state = vec!["P"; reps].join(" ");
    let req = json!({"model": "m", "state": state, "questions": {"q": {"type": "noul"}},
        "assets": {"P": asset}});
    assert_eq!(code(to_oa(&req)), Some("too_long"));
    // A structured state.
    let req = json!({"model": "m", "state": {"frames": vec!["P"; reps]},
        "questions": {"q": {"type": "noul"}}, "assets": {"P": asset}});
    assert_eq!(code(to_oa(&req)), Some("too_long"));
    // Clef images count toward the same budget.
    let req = json!({"model": "m", "state": vec!["P"; reps - 1].join(" "),
        "questions": {"q": {"type": "noul"}}, "assets": {"P": asset},
        "images": [format!("data:image/png;base64,{png}"), format!("data:image/png;base64,{png}")]});
    assert_eq!(code(to_oa(&req)), Some("too_long"));
    // Under the budget, repeats still translate.
    let req = json!({"model": "m", "state": "P P P", "questions": {"q": {"type": "noul"}},
        "assets": {"P": asset}});
    let out = body(&to_oa(&req).unwrap());
    let parts = out["input"][0]["content"].as_array().unwrap();
    assert_eq!(
        parts.iter().filter(|p| p["type"] == "input_image").count(),
        3
    );
}

// Review R3-4.1: many questions with one name must not make the suffix
// search quadratic.
#[test]
fn many_duplicate_names_get_suffixes_fast() {
    let n = 100_000;
    let mut questions: Vec<Value> = (0..n)
        .map(|_| json!({"type": "predicate", "name": "a", "instructions": "x"}))
        .collect();
    // A client name that looks like a suffix stays reserved for its question.
    questions.push(json!({"type": "predicate", "name": "a_5", "instructions": "x"}));
    let t0 = std::time::Instant::now();
    let names = target_names(&questions).unwrap();
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(names[0].0, "a");
    assert_eq!(names[1].0, "a_1");
    assert_eq!(names[4].0, "a_4");
    assert_eq!(names[5].0, "a_6", "a_5 is reserved");
    assert_eq!(names[n].0, "a_5");
    let unique: HashSet<&str> = names.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(unique.len(), names.len());
}

// Review R3-4.2: OpenAI takes at most 128 images; every reference is one.
#[test]
fn more_than_128_image_occurrences_do_not_translate_to_openai() {
    let asset = json!({"type": "image", "format": "png", "data_base64": "iVBORw0KGgoAAAA"});
    let req = |n: usize| {
        json!({"model": "m", "state": vec!["P"; n].join(" "),
            "questions": {"q": {"type": "noul"}}, "assets": {"P": asset}})
    };
    let out = body(&to_oa(&req(128)).unwrap());
    let parts = out["input"][0]["content"].as_array().unwrap();
    assert_eq!(
        parts.iter().filter(|p| p["type"] == "input_image").count(),
        128
    );
    assert_eq!(to_oa(&req(129)).unwrap_err().code(), "unsupported");
    // Clef images count too.
    let mut r = req(128);
    r["images"] = json!(["data:image/png;base64,iVBORw0KGgoAAAA"]);
    assert_eq!(to_oa(&r).unwrap_err().code(), "unsupported");
    // A huge number of references fails fast (split lazily).
    let t0 = std::time::Instant::now();
    assert_eq!(to_oa(&req(500_000)).unwrap_err().code(), "unsupported");
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        t0.elapsed()
    );
}

#[test]
fn image_occurrences_count_every_reference() {
    let asset = json!({"type": "image", "format": "png", "data_base64": "iVBORw0KGgoAAAA"});
    let count = |v: Value| openai_image_occurrences(&serde_json::to_vec(&v).unwrap());
    let state = vec!["P"; 128].join(" ");
    assert_eq!(
        count(json!({"model": "m", "state": state, "questions": {}, "assets": {"P": asset}})),
        Some(128)
    );
    assert_eq!(
        count(
            json!({"model": "m", "state": {"a": ["P", "xPx"]}, "questions": {},
            "assets": {"P": asset}, "images": ["data:image/png;base64,AA"]})
        ),
        Some(3)
    );
    // Counting stops just over the limit.
    let state = vec!["P"; 10_000].join(" ");
    assert_eq!(
        count(json!({"model": "m", "state": state, "questions": {}, "assets": {"P": asset}})),
        Some(MAX_OPENAI_IMAGES as u32 + 1)
    );
    assert_eq!(
        count(json!({"model": "m", "state": "P", "questions": {}})),
        None
    );
}
