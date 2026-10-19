//! Sanitized global/project Ollama Cloud sampling and model-cap configuration.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use pa_types::ai::Model;
use serde_json::{Map, Value};

const NUMERIC_INFERENCE_KEYS: &[&str] = &[
    "temperature",
    "top_p",
    "top_k",
    "min_p",
    "typical_p",
    "tfs_z",
    "repeat_penalty",
    "repeat_last_n",
    "seed",
    "num_predict",
    "num_ctx",
    "num_gpu",
    "num_thread",
    "mirostat",
    "mirostat_tau",
    "mirostat_eta",
    "frequency_penalty",
    "presence_penalty",
    "max_tokens",
];
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

type Params = Map<String, Value>;
type PerModel = BTreeMap<String, Params>;

/// A sanitized snapshot. Loading never reads environment variables or credentials.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OllamaCloudConfig {
    inference_params: Params,
    models: PerModel,
    model_overrides: PerModel,
}

impl OllamaCloudConfig {
    /// Load global then project configuration, merging model entries per key.
    /// Missing or invalid files fall back independently; read/parse errors are logged.
    #[must_use]
    pub fn load(cwd: &Path, agent_dir: &Path) -> Self {
        let mut config = read_config(&agent_dir.join("ollama-cloud.json"));
        let project = read_config(&cwd.join(".prime/agent/ollama-cloud.json"));
        config.inference_params.extend(project.inference_params);
        merge_per_model(&mut config.models, project.models);
        merge_per_model(&mut config.model_overrides, project.model_overrides);
        config
    }

    /// Overlay allowed sampling values onto an object body; other JSON passes through.
    /// Per-model parameters win over shared defaults and existing request values.
    #[must_use]
    pub fn overlay_request(&self, mut payload: Value, model_id: &str) -> Value {
        if let Some(body) = payload.as_object_mut() {
            body.extend(self.inference_params.clone());
            if let Some(params) = self.models.get(model_id) {
                body.extend(params.clone());
            }
        }
        payload
    }

    /// Clone a model and apply configured caps without changing the catalog model.
    /// Other providers remain unchanged even when their model IDs match.
    #[must_use]
    pub fn configure_model(&self, model: &Model) -> Model {
        let mut result = model.clone();
        if model.provider != "ollama-cloud" {
            return result;
        }
        if let Some(caps) = self.model_overrides.get(&model.id) {
            if let Some(context_window) = caps.get("contextWindow").and_then(Value::as_u64) {
                result.context_window = context_window;
            }
            if let Some(max_tokens) = caps.get("maxTokens").and_then(Value::as_u64) {
                result.max_tokens = max_tokens;
                // Explicit caps must bypass the provider's default output ceiling.
                result.max_tokens_explicit = true;
            }
        }
        result
    }
}

fn read_config(path: &Path) -> OllamaCloudConfig {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) => {
            if error.kind() != ErrorKind::NotFound {
                eprintln!(
                    "[prime-ollama-cloud] Failed to load config from {}: {error}",
                    path.display()
                );
            }
            return OllamaCloudConfig::default();
        }
    };
    // JSON.parse accepts overflow as Infinity. serde_json rejects it, which would
    // discard valid sibling settings. Normalize JSON numbers first, never strings.
    let parsed = match serde_json::from_str::<Value>(&normalize_json_numbers(&content)) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!(
                "[prime-ollama-cloud] Failed to load config from {}: {error}",
                path.display()
            );
            return OllamaCloudConfig::default();
        }
    };
    let Some(raw) = parsed.as_object() else {
        return OllamaCloudConfig::default();
    };
    OllamaCloudConfig {
        inference_params: raw
            .get("inferenceParams")
            .and_then(Value::as_object)
            .map(sanitize_inference_params)
            .unwrap_or_default(),
        models: sanitize_per_model(raw.get("models"), sanitize_inference_params),
        model_overrides: sanitize_per_model(raw.get("modelOverrides"), sanitize_model_overrides),
    }
}

fn sanitize_inference_params(raw: &Params) -> Params {
    let mut result = Params::new();
    for &key in NUMERIC_INFERENCE_KEYS {
        if let Some(value) = raw
            .get(key)
            .filter(|value| value.as_f64().is_some_and(f64::is_finite))
        {
            result.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(stop) = raw.get("stop").filter(|stop| {
        stop.is_string()
            || stop
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string))
    }) {
        result.insert("stop".to_owned(), stop.clone());
    }
    result
}

fn sanitize_model_overrides(raw: &Params) -> Params {
    let mut result = Params::new();
    for key in ["contextWindow", "maxTokens"] {
        if let Some(value) = raw
            .get(key)
            .and_then(Value::as_f64)
            .filter(|value| *value > 0.0 && *value <= MAX_SAFE_INTEGER && value.fract() == 0.0)
        {
            // The range and integral checks prove this conversion is lossless.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let cap = value as u64;
            result.insert(key.to_owned(), Value::from(cap));
        }
    }
    result
}

fn sanitize_per_model(raw: Option<&Value>, sanitize: fn(&Params) -> Params) -> PerModel {
    let mut result = PerModel::new();
    if let Some(models) = raw.and_then(Value::as_object) {
        for (id, value) in models {
            if let Some(raw_params) = value.as_object() {
                let params = sanitize(raw_params);
                if !params.is_empty() {
                    result.insert(id.clone(), params);
                }
            }
        }
    }
    result
}

fn merge_per_model(base: &mut PerModel, project: PerModel) {
    for (id, params) in project {
        base.entry(id).or_default().extend(params);
    }
}

/// Match JSON's strict number grammar, not Rust's more permissive float parser.
fn is_json_number(token: &str) -> bool {
    let mut bytes = token.bytes().peekable();
    if bytes.peek() == Some(&b'-') {
        bytes.next();
    }
    match bytes.next() {
        Some(b'0') => {}
        Some(b'1'..=b'9') => {
            while bytes.peek().is_some_and(u8::is_ascii_digit) {
                bytes.next();
            }
        }
        _ => return false,
    }
    if bytes.peek() == Some(&b'.') {
        bytes.next();
        if !bytes.next().is_some_and(|byte| byte.is_ascii_digit()) {
            return false;
        }
        while bytes.peek().is_some_and(u8::is_ascii_digit) {
            bytes.next();
        }
    }
    if matches!(bytes.peek(), Some(b'e' | b'E')) {
        bytes.next();
        if matches!(bytes.peek(), Some(b'+' | b'-')) {
            bytes.next();
        }
        if !bytes.next().is_some_and(|byte| byte.is_ascii_digit()) {
            return false;
        }
        while bytes.peek().is_some_and(u8::is_ascii_digit) {
            bytes.next();
        }
    }
    bytes.next().is_none()
}

fn normalize_json_numbers(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut index = 0;
    let mut copied = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
        } else if byte == b'"' {
            in_string = true;
            index += 1;
        } else if byte == b'-' || byte.is_ascii_digit() {
            let start = index;
            while index < bytes.len()
                && !matches!(
                    bytes[index],
                    b' ' | b'\t' | b'\r' | b'\n' | b',' | b':' | b'[' | b']' | b'{' | b'}' | b'"'
                )
            {
                index += 1;
            }
            let token = &input[start..index];
            if let (true, Ok(number)) = (is_json_number(token), token.parse::<f64>()) {
                output.push_str(&input[copied..start]);
                if !number.is_finite() {
                    // Null and Infinity are both rejected by the config sanitizer.
                    output.push_str("null");
                } else if number.fract() == 0.0 && number.abs() <= MAX_SAFE_INTEGER {
                    // JS has one numeric type; normalize integers after f64 rounding.
                    #[allow(clippy::cast_possible_truncation)]
                    let integer = number as i64;
                    output.push_str(&integer.to_string());
                } else if let Some(number) = serde_json::Number::from_f64(number) {
                    output.push_str(&number.to_string());
                }
                copied = index;
            }
        } else {
            index += 1;
        }
    }
    output.push_str(&input[copied..]);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    struct Fixture {
        _root: TempDir,
        cwd: std::path::PathBuf,
        agent_dir: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let cwd = root.path().join("project");
            let agent_dir = root.path().join("global");
            fs::create_dir_all(cwd.join(".prime/agent")).unwrap();
            fs::create_dir_all(&agent_dir).unwrap();
            Self {
                _root: root,
                cwd,
                agent_dir,
            }
        }

        fn global(&self, config: &Value) {
            fs::write(self.agent_dir.join("ollama-cloud.json"), config.to_string()).unwrap();
        }

        fn project(&self, config: &Value) {
            fs::write(
                self.cwd.join(".prime/agent/ollama-cloud.json"),
                config.to_string(),
            )
            .unwrap();
        }

        fn load(&self) -> OllamaCloudConfig {
            OllamaCloudConfig::load(&self.cwd, &self.agent_dir)
        }
    }

    fn model(id: &str) -> Model {
        serde_json::from_value(json!({
            "id": id, "name": "Catalog model", "api": "openai-completions",
            "provider": "ollama-cloud", "baseUrl": "https://ollama.com/v1",
            "reasoning": true, "input": ["text", "image"],
            "contextWindow": 200_000, "maxTokens": 32000,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "headers": {"X-Test": "unchanged"}, "featured": true,
            "compat": {"supportsDeveloperRole": false}
        }))
        .unwrap()
    }

    #[test]
    fn merges_each_scope_per_key_and_only_overlays_the_outgoing_body() {
        let fixture = Fixture::new();
        fixture.global(&json!({
            "inferenceParams": {"temperature": 0.2, "top_p": 0.9, "seed": 12},
            "models": {"glm-5": {"top_k": 10, "temperature": 0.4}, "other": {"seed": 20}},
            "modelOverrides": {
                "glm-5": {"contextWindow": 300_000, "maxTokens": 32000},
                "other": {"maxTokens": 1000}
            }
        }));
        fixture.project(&json!({
            "inferenceParams": {"temperature": 0.3},
            "models": {"glm-5": {"top_k": 25, "stop": ["END"]}},
            "modelOverrides": {"glm-5": {"maxTokens": 16000}}
        }));
        let config = fixture.load();
        assert_eq!(config.models.get("other"), json!({"seed": 20}).as_object());
        assert_eq!(
            config.overlay_request(json!({}), "unknown"),
            json!({
                "temperature": 0.3, "top_p": 0.9, "seed": 12
            })
        );
        let body = json!({
            "model": "glm-5", "messages": [{"role": "user", "content": "hi"}],
            "temperature": 1, "stream": true
        });
        let original = body.clone();
        assert_eq!(
            config.overlay_request(body.clone(), "glm-5"),
            json!({
                "model": "glm-5", "messages": [{"role": "user", "content": "hi"}],
                "temperature": 0.4, "stream": true, "top_p": 0.9, "seed": 12,
                "top_k": 25, "stop": ["END"]
            })
        );
        assert_eq!(body, original);
        let catalog = model("glm-5");
        let mut expected = catalog.clone();
        expected.context_window = 300_000;
        expected.max_tokens = 16_000;
        expected.max_tokens_explicit = true;
        assert_eq!(config.configure_model(&catalog), expected);
        assert_eq!(catalog, model("glm-5"));
        let absent = model("absent");
        assert_eq!(config.configure_model(&absent), absent);
        let mut other_provider = catalog;
        other_provider.provider = "other-provider".into();
        assert_eq!(config.configure_model(&other_provider), other_provider);
    }

    #[test]
    fn drops_unknown_fields_and_invalid_values_before_merging() {
        let fixture = Fixture::new();
        fixture.global(&json!({
            "inferenceParams": {
                "temperature": "0.4", "top_p": 0.8, "seed": null,
                "stop": ["ok", 3], "model": "hijack", "messages": [], "max_tokens": 4096
            },
            "models": {
                "glm-5": {"top_k": "100", "stop": "DONE", "stream": false},
                "ignored": [], "empty": {"foo": 1}
            },
            "modelOverrides": {
                "glm-5": {"contextWindow": -1, "maxTokens": 1.2, "api": "hijack"},
                "good": {"contextWindow": 50000, "maxTokens": 2000},
                "invalid": {"maxTokens": 0, "contextWindow": "100"}
            }
        }));
        // Invalid project values cannot clear a valid global value.
        fixture.project(&json!({
            "inferenceParams": {"top_p": false, "max_tokens": {}, "stop": null},
            "models": {"glm-5": {"stop": [true]}},
            "modelOverrides": {"good": {"contextWindow": null, "maxTokens": []}}
        }));
        let config = fixture.load();
        assert_eq!(
            config.inference_params,
            json!({"top_p": 0.8, "max_tokens": 4096})
                .as_object()
                .unwrap()
                .clone()
        );
        assert_eq!(
            config.models,
            BTreeMap::from([(
                "glm-5".to_owned(),
                json!({"stop": "DONE"}).as_object().unwrap().clone()
            )])
        );
        assert_eq!(
            config.model_overrides,
            BTreeMap::from([(
                "good".to_owned(),
                json!({"contextWindow": 50000, "maxTokens": 2000})
                    .as_object()
                    .unwrap()
                    .clone()
            )])
        );
        assert_eq!(
            config.overlay_request(
                json!({"model": "real", "messages": [], "stream": true}),
                "glm-5"
            ),
            json!({
                "model": "real", "messages": [], "stream": true,
                "top_p": 0.8, "max_tokens": 4096, "stop": "DONE"
            })
        );
        for payload in [Value::Null, json!([]), json!("text"), json!(3), json!(true)] {
            assert_eq!(config.overlay_request(payload.clone(), "glm-5"), payload);
        }
    }

    #[test]
    fn accepts_all_finite_sampler_keys_without_imposing_numeric_ranges() {
        let fixture = Fixture::new();
        fixture.global(&json!({"inferenceParams": {
            "temperature": -2, "top_p": 1.5, "top_k": -0.5, "min_p": 0,
            "typical_p": 0.2, "tfs_z": 0.3, "repeat_penalty": -1,
            "repeat_last_n": -1, "seed": -10, "num_predict": -2, "num_ctx": 0,
            "num_gpu": -1, "num_thread": 2.5, "mirostat": 3,
            "mirostat_tau": 0.01, "mirostat_eta": 0.02,
            "frequency_penalty": -0.4, "presence_penalty": 0.4,
            "max_tokens": -1, "stop": []
        }}));
        assert_eq!(
            fixture.load().overlay_request(json!({}), "any"),
            json!({
                "temperature": -2, "top_p": 1.5, "top_k": -0.5, "min_p": 0,
                "typical_p": 0.2, "tfs_z": 0.3, "repeat_penalty": -1,
                "repeat_last_n": -1, "seed": -10, "num_predict": -2, "num_ctx": 0,
                "num_gpu": -1, "num_thread": 2.5, "mirostat": 3,
                "mirostat_tau": 0.01, "mirostat_eta": 0.02,
                "frequency_penalty": -0.4, "presence_penalty": 0.4,
                "max_tokens": -1, "stop": []
            })
        );
    }

    #[test]
    fn rejects_overflow_numbers_but_keeps_valid_siblings_and_empty_stop() {
        let fixture = Fixture::new();
        fs::write(fixture.agent_dir.join("ollama-cloud.json"), concat!(
            r#"{"inferenceParams":{"temperature":1e999,"top_p":0.6,"stop":["ok",null]},"#,
            r#""models":{"glm-5":{"seed":-1e999,"stop":[]}},"#,
            r#""modelOverrides":{"glm-5":{"contextWindow":1e999,"maxTokens":9007199254740992}}}"#
        )).unwrap();
        let config = fixture.load();
        assert!(config.model_overrides.is_empty());
        assert_eq!(
            config.overlay_request(json!({"model": "glm-5", "temperature": 1}), "glm-5"),
            json!({
                "model": "glm-5", "temperature": 1, "top_p": 0.6, "stop": []
            })
        );
    }

    #[test]
    fn caps_use_js_safe_integer_semantics_including_decimal_rounding() {
        let fixture = Fixture::new();
        fs::write(fixture.agent_dir.join("ollama-cloud.json"), concat!(
            r#"{"modelOverrides":{"glm-5":{"contextWindow":9007199254740991.1,"maxTokens":2e3},"#,
            r#""unsafe":{"contextWindow":9007199254740992,"maxTokens":-0},"#,
            r#""invalid":{"contextWindow":true,"maxTokens":0.5}},"#,
            r#""inferenceParams":{"seed":9007199254740993,"num_ctx":1e-999}}"#
        )).unwrap();
        let config = fixture.load();
        let mut expected = model("glm-5");
        expected.context_window = 9_007_199_254_740_991;
        expected.max_tokens = 2000;
        expected.max_tokens_explicit = true;
        assert_eq!(config.configure_model(&model("glm-5")), expected);
        assert_eq!(config.configure_model(&model("unsafe")), model("unsafe"));
        assert_eq!(config.configure_model(&model("invalid")), model("invalid"));
        assert_eq!(
            config.overlay_request(json!({}), "any"),
            json!({
                "seed": 9_007_199_254_740_992.0, "num_ctx": 0
            })
        );
    }

    #[test]
    fn invalid_or_missing_files_fall_back_independently() {
        let fixture = Fixture::new();
        assert_eq!(fixture.load(), OllamaCloudConfig::default());
        fs::write(fixture.agent_dir.join("ollama-cloud.json"), "{bad json").unwrap();
        fixture.project(&json!({"inferenceParams": {"seed": 1}}));
        assert_eq!(
            fixture.load().overlay_request(json!({}), "any"),
            json!({"seed": 1})
        );
        fixture.global(&json!({"inferenceParams": {"temperature": 0.5}}));
        for non_object in [json!(["not a config"]), Value::Null, json!(false), json!(1)] {
            fixture.project(&non_object);
            assert_eq!(
                fixture.load().overlay_request(json!({}), "any"),
                json!({"temperature": 0.5})
            );
        }
        fixture.global(&Value::Null);
        assert_eq!(fixture.load(), OllamaCloudConfig::default());
        let body = json!({"model": "x", "temperature": 1});
        assert_eq!(fixture.load().overlay_request(body.clone(), "x"), body);
    }

    #[test]
    fn numeric_normalization_preserves_strings_and_rejects_malformed_numbers() {
        let fixture = Fixture::new();
        fixture.global(&json!({"inferenceParams": {"stop": ["1e999", "雪\\\"1e999", "-01e999"]}}));
        assert_eq!(
            fixture.load().overlay_request(json!({}), "any"),
            json!({
                "stop": ["1e999", "雪\\\"1e999", "-01e999"]
            })
        );
        for malformed in [
            "01e999",
            "-01e999",
            ".1",
            "1.",
            "1e",
            "1e+",
            "NaN",
            "Infinity",
            "1e999true",
            "+1",
        ] {
            fs::write(
                fixture.agent_dir.join("ollama-cloud.json"),
                format!(r#"{{"inferenceParams":{{"temperature":{malformed},"seed":1}}}}"#),
            )
            .unwrap();
            assert_eq!(fixture.load(), OllamaCloudConfig::default(), "{malformed}");
        }
    }

    #[test]
    fn independent_loads_and_reload_do_not_retain_removed_caps() {
        let fixture = Fixture::new();
        fixture.project(&json!({"modelOverrides": {"glm-5": {"contextWindow": 4000}}}));
        let first = fixture.load();
        fixture.project(&json!({"modelOverrides": {"glm-5": {"contextWindow": 8000}}}));
        let second = fixture.load();
        fixture.project(&json!({}));
        let catalog = model("glm-5");
        let mut expected_first = catalog.clone();
        expected_first.context_window = 4000;
        let mut expected_second = catalog.clone();
        expected_second.context_window = 8000;
        assert_eq!(first.configure_model(&catalog), expected_first);
        assert_eq!(second.configure_model(&catalog), expected_second);
        assert_eq!(fixture.load().configure_model(&catalog), catalog);
    }
}
