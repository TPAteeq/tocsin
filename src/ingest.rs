use serde_json::{Map, Value};

const MESSAGE: [&str; 5] = ["message", "msg", "log", "body", "text"];
const LEVEL: [&str; 4] = ["level", "severity", "severityText", "lvl"];
const SERVICE: [&str; 3] = ["service", "service.name", "app"];
const EVENT: [&str; 3] = ["exception.type", "exception.message", "event.name"];
const MAX_FIELD: usize = 128;
const MAX_LINE: usize = 16 * 1024;

pub fn otlp(body: &[u8]) -> anyhow::Result<Vec<String>> {
    let doc: Value = serde_json::from_slice(body)?;
    let mut lines = Vec::new();
    for resource in items(&doc, "resourceLogs") {
        let service = resource
            .pointer("/resource/attributes")
            .and_then(|a| attribute(a, "service.name"));
        for scope in items(resource, "scopeLogs") {
            for record in items(scope, "logRecords") {
                let body = record
                    .get("body")
                    .map(any_value)
                    .filter(|b| !b.trim().is_empty())
                    .or_else(|| event(record.get("attributes")?));
                let Some(body) = body else { continue };
                let level = record
                    .get("severityText")
                    .and_then(Value::as_str)
                    .or_else(|| severity_name(record.get("severityNumber")?.as_u64()?));
                lines.push(join([service.as_deref(), level, Some(&body)]));
            }
        }
    }
    Ok(lines)
}

pub fn lines(body: &str) -> Vec<String> {
    body.lines().filter_map(normalize).collect()
}

pub fn normalize(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if line.starts_with('{') {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(line) {
            if let Some(message) = field(&map, &MESSAGE) {
                return Some(join([
                    field(&map, &SERVICE),
                    field(&map, &LEVEL),
                    Some(message),
                ]));
            }
        }
    }
    Some(truncate(line, MAX_LINE).to_string())
}

fn severity_name(number: u64) -> Option<&'static str> {
    const NAMES: [&str; 6] = ["TRACE", "DEBUG", "INFO", "WARN", "ERROR", "FATAL"];
    NAMES.get(number.checked_sub(1)? as usize / 4).copied()
}

fn items<'a>(value: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn attribute(attributes: &Value, key: &str) -> Option<String> {
    attributes
        .as_array()?
        .iter()
        .find(|a| a.get("key").and_then(Value::as_str) == Some(key))
        .and_then(|a| a.get("value"))
        .map(any_value)
}

fn event(attributes: &Value) -> Option<String> {
    let parts: Vec<String> = EVENT
        .iter()
        .filter_map(|k| attribute(attributes, k))
        .collect();
    (!parts.is_empty()).then(|| parts.join(": "))
}

fn any_value(value: &Value) -> String {
    let Value::Object(map) = value else {
        return value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_string);
    };
    [
        "stringValue",
        "intValue",
        "doubleValue",
        "boolValue",
        "kvlistValue",
        "arrayValue",
        "bytesValue",
    ]
    .iter()
    .find_map(|k| map.get(*k))
    .map_or_else(String::new, |v| {
        v.as_str().map_or_else(|| v.to_string(), str::to_string)
    })
}

fn field<'a>(map: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| map.get(*k)?.as_str())
        .filter(|s| !s.is_empty())
}

fn join(parts: [Option<&str>; 3]) -> String {
    let [service, level, message] = parts.map(|p| p.unwrap_or_default());
    [
        truncate(service, MAX_FIELD),
        truncate(level, MAX_FIELD),
        truncate(message, MAX_LINE),
    ]
    .into_iter()
    .filter(|p| !p.is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_otlp_json() {
        let body = br#"{"resourceLogs":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"checkout"}}]},
            "scopeLogs":[{"logRecords":[
                {"severityText":"ERROR","body":{"stringValue":"payment declined"}},
                {"body":{"kvlistValue":{"values":[]}}},
                {"body":{"stringValue":"  "}},
                {"severityNumber":21,"body":{"stringValue":"disk gone"}},
                {"severityText":"ERROR","attributes":[
                    {"key":"exception.type","value":{"stringValue":"TimeoutError"}},
                    {"key":"exception.message","value":{"stringValue":"upstream took 30s"}}
                ]}
            ]}]}]}"#;
        assert_eq!(
            otlp(body).unwrap(),
            vec![
                "checkout ERROR payment declined",
                r#"checkout {"values":[]}"#,
                "checkout FATAL disk gone",
                "checkout ERROR TimeoutError: upstream took 30s",
            ]
        );
    }

    #[test]
    fn caps_field_sizes() {
        let service = "s".repeat(10_000);
        let body = format!(
            r#"{{"resourceLogs":[{{"resource":{{"attributes":[{{"key":"service.name","value":{{"stringValue":"{service}"}}}}]}},"scopeLogs":[{{"logRecords":[{{"body":{{"stringValue":"ok"}}}}]}}]}}]}}"#
        );
        let lines = otlp(body.as_bytes()).unwrap();
        assert_eq!(lines[0].len(), MAX_FIELD + " ok".len());
        assert_eq!(normalize(&"é".repeat(MAX_LINE)).unwrap().len(), MAX_LINE);
    }

    #[test]
    fn normalizes_structured_and_plain_lines() {
        let body = "{\"level\":\"warn\",\"msg\":\"cache miss\",\"service\":\"api\"}\nplain text line\n\n{\"no\":\"message\"}";
        assert_eq!(
            lines(body),
            vec![
                "api warn cache miss",
                "plain text line",
                "{\"no\":\"message\"}"
            ]
        );
    }
}
