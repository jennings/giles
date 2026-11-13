//! Volume sizes from the engine's disk usage report, `GET /system/df`.
//!
//! The report comes in two shapes. API 1.52 (Docker 29) and later put the volumes under
//! `VolumeUsage.Items`, and only when the request sets `verbose`. Older engines list them in a
//! top-level `Volumes` array. Podman's compatible API sends the newer shape whatever it is asked.
//! bollard models only the newer shape, so a negotiated older engine's sizes would be dropped: the
//! report is read here as plain JSON, over the same socket, at the negotiated API version.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use bollard::ClientVersion;
use http_body_util::{BodyExt as _, Empty};
use hyper::Request;
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;

/// Reads the report from the engine at `socket` and returns the size of each volume, by name.
pub(crate) async fn volume_sizes(
    socket: &Path,
    version: ClientVersion,
    timeout: Duration,
) -> Result<HashMap<String, u64>, String> {
    let report = tokio::time::timeout(timeout, report(socket, version))
        .await
        .map_err(|_| format!("no answer within {}s", timeout.as_secs()))??;
    Ok(sizes(&report))
}

async fn report(socket: &Path, version: ClientVersion) -> Result<serde_json::Value, String> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|e| format!("connecting: {e}"))?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| format!("starting HTTP: {e}"))?;
    let connection = tokio::spawn(connection);
    let request = Request::get(format!("/v{version}/system/df?verbose=1"))
        .header(hyper::header::HOST, "docker")
        .body(Empty::<Bytes>::new())
        .map_err(|e| e.to_string())?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|e| format!("requesting: {e}"))?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("reading: {e}"))?
        .to_bytes();
    connection.abort();
    if !status.is_success() {
        return Err(format!(
            "status {status}: {}",
            String::from_utf8_lossy(&body)
        ));
    }
    serde_json::from_slice(&body).map_err(|e| format!("not JSON: {e}"))
}

/// The measured size of each volume in `report`, in either shape. A volume the engine hasn't
/// measured (size -1, or none) is left out.
fn sizes(report: &serde_json::Value) -> HashMap<String, u64> {
    let newer = report.pointer("/VolumeUsage/Items");
    let older = report.get("Volumes");
    newer
        .or(older)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|volume| {
            let name = volume.get("Name")?.as_str()?.to_owned();
            let size = volume.pointer("/UsageData/Size")?.as_i64()?;
            u64::try_from(size).ok().map(|size| (name, size))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_the_volumes_of_both_report_shapes() {
        let volume = |name: &str, size: i64| json!({"Name": name, "UsageData": {"Size": size, "RefCount": 1}});
        // API 1.52 and later, with `verbose`.
        let newer =
            json!({"VolumeUsage": {"TotalSize": 7, "Items": [volume("a", 3), volume("b", 4)]}});
        // Before API 1.52.
        let older = json!({"LayersSize": 1, "Volumes": [volume("a", 3), volume("b", 4)]});
        let expected = HashMap::from([("a".to_owned(), 3), ("b".to_owned(), 4)]);
        assert_eq!(sizes(&newer), expected);
        assert_eq!(sizes(&older), expected);
    }

    #[test]
    fn unmeasured_and_missing_sizes_are_left_out() {
        let report = json!({"Volumes": [
            {"Name": "unmeasured", "UsageData": {"Size": -1}},
            {"Name": "no-usage"},
            {"UsageData": {"Size": 5}},
            {"Name": "measured", "UsageData": {"Size": 0}},
        ]});
        assert_eq!(sizes(&report), HashMap::from([("measured".to_owned(), 0)]));
        // The newer shape without `verbose` has totals only.
        assert!(sizes(&json!({"VolumeUsage": {"TotalSize": 7}})).is_empty());
    }
}
