//! Minimal Prometheus instant-query client.
//!
//! Only extracts what the remediator needs: the `device` label of every
//! series a rule's PromQL expression returns. The "sustained" logic
//! (hysteresis) lives entirely in the PromQL itself (e.g. `max_over_time`
//! over a window), so this client only has to run one instant query per
//! rule per poll and read off which devices came back.

use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct QueryResponse {
    status: String,
    #[serde(default)]
    data: Option<QueryData>,
}

#[derive(Debug, Deserialize)]
struct QueryData {
    #[serde(default)]
    result: Vec<ResultItem>,
}

#[derive(Debug, Deserialize)]
struct ResultItem {
    #[serde(default)]
    metric: HashMap<String, String>,
}

/// Parse a Prometheus `/api/v1/query` response body and return the deduped
/// `device` label of every series in the result vector. Pure (no I/O),
/// which is what makes it unit-testable without a live Prometheus.
///
/// - `status != "success"` -> `Err`.
/// - A result item with no `device` label is skipped (not an error).
/// - An empty result vector -> `Ok(vec![])`.
pub fn parse_query_devices(json: &str) -> Result<Vec<String>, String> {
    let resp: QueryResponse = serde_json::from_str(json).map_err(|e| format!("invalid prometheus response json: {e}"))?;
    if resp.status != "success" {
        return Err(format!("prometheus query returned status={:?}", resp.status));
    }
    let data = resp.data.ok_or_else(|| "prometheus response missing \"data\"".to_string())?;

    let mut devices = Vec::new();
    for item in data.result {
        if let Some(device) = item.metric.get("device") {
            if !devices.contains(device) {
                devices.push(device.clone());
            }
        }
    }
    Ok(devices)
}

/// GET `{base}/api/v1/query?query=<expr>` and return the deduped `device`
/// labels of the result vector. Non-2xx HTTP or a non-"success" Prometheus
/// status both surface as `Err` (never a panic).
pub async fn query_devices(http: &reqwest::Client, base: &str, expr: &str) -> Result<Vec<String>, String> {
    let url = format!("{}/api/v1/query", base.trim_end_matches('/'));
    let resp = http
        .get(&url)
        .query(&[("query", expr)])
        .send()
        .await
        .map_err(|e| format!("prometheus request failed: {e}"))?;

    let status = resp.status();
    let body = resp.text().await.map_err(|e| format!("reading prometheus response failed: {e}"))?;
    if !status.is_success() {
        return Err(format!("prometheus http status {status}: {body}"));
    }
    parse_query_devices(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_deduped_device_labels_and_skips_missing_ones() {
        let json = r#"
        {
          "status": "success",
          "data": {
            "resultType": "vector",
            "result": [
              {"metric": {"__name__": "gocoax_up", "device": "ff"}, "value": [1690000000, "0"]},
              {"metric": {"__name__": "gocoax_up", "device": "gg"}, "value": [1690000000, "0"]},
              {"metric": {"__name__": "gocoax_up"}, "value": [1690000000, "0"]},
              {"metric": {"__name__": "gocoax_up", "device": "ff"}, "value": [1690000060, "0"]}
            ]
          }
        }
        "#;
        let devices = parse_query_devices(json).unwrap();
        assert_eq!(devices, vec!["ff".to_string(), "gg".to_string()]);
    }

    #[test]
    fn empty_result_vector_yields_empty_devices() {
        let json = r#"{"status":"success","data":{"resultType":"vector","result":[]}}"#;
        assert_eq!(parse_query_devices(json).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn error_status_is_an_error() {
        let json = r#"{"status":"error","errorType":"bad_data","error":"parse error"}"#;
        assert!(parse_query_devices(json).is_err());
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(parse_query_devices("not json").is_err());
    }
}
