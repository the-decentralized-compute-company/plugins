//! Rendering one event into the body of one POST.
//!
//! Three shapes are supported because they are the three that actually land
//! somewhere: a generic JSON envelope for your own receiver, a Slack incoming
//! webhook, and a Discord channel webhook. All three are pure functions of the
//! event, so every claim about a payload is testable without a network.
//!
//! Payload size is bounded on purpose: Discord rejects embeds over roughly
//! 6000 characters, and a peer serving two hundred models would otherwise turn
//! into an undeliverable message that retries forever.

use serde_json::{Value, json};

use crate::config::PayloadFormat;
use crate::event::{Delivery, EventKind, NodeEvent, Severity, short_id};

/// Longest single field value written into a payload.
const MAX_FIELD_CHARS: usize = 900;

pub fn render(format: PayloadFormat, delivery: &Delivery, max_list: usize) -> Value {
    match format {
        PayloadFormat::Json => render_json(delivery, max_list),
        PayloadFormat::Slack => render_slack(delivery, max_list),
        PayloadFormat::Discord => render_discord(delivery, max_list),
    }
}

fn render_json(delivery: &Delivery, max_list: usize) -> Value {
    let event = &delivery.event;
    json!({
        "source": "tdcc",
        "event": event.kind.as_str(),
        "severity": severity_name(event.kind.severity()),
        "summary": summary_line(event, delivery.suppressed),
        "timestamp": format_rfc3339_utc(event.timestamp_ms),
        "timestamp_ms": event.timestamp_ms,
        "node_id": event.node_id,
        "mesh_id": event.mesh_id,
        "model": event.model,
        "peer": event.peer_json(max_list),
        // How many identical events the coalescer dropped since this key was
        // last delivered. Always present so a receiver can alert on it.
        "coalesced": delivery.suppressed,
        "detail": event.detail,
    })
}

fn render_slack(delivery: &Delivery, max_list: usize) -> Value {
    let event = &delivery.event;
    let summary = summary_line(event, delivery.suppressed);
    let fields: Vec<Value> = detail_fields(delivery, max_list)
        .into_iter()
        .map(|(name, value)| json!({ "title": name, "value": value, "short": true }))
        .collect();

    json!({
        "text": format!("TDCC · {summary}"),
        "attachments": [{
            "color": slack_color(event.kind.severity()),
            "fallback": summary,
            "fields": fields,
            "footer": format!("tdcc · node {}", short_id(&event.node_id)),
            "ts": event.timestamp_ms / 1000,
        }],
    })
}

fn render_discord(delivery: &Delivery, max_list: usize) -> Value {
    let event = &delivery.event;
    let fields: Vec<Value> = detail_fields(delivery, max_list)
        .into_iter()
        .map(|(name, value)| json!({ "name": name, "value": value, "inline": true }))
        .collect();

    json!({
        "username": "TDCC",
        "embeds": [{
            "title": event.kind.as_str(),
            "description": summary_line(event, delivery.suppressed),
            "color": discord_color(event.kind.severity()),
            "fields": fields,
            "footer": { "text": format!("node {}", short_id(&event.node_id)) },
            "timestamp": format_rfc3339_utc(event.timestamp_ms),
        }],
    })
}

/// One line a human reads in a channel and immediately understands.
pub fn summary_line(event: &NodeEvent, suppressed: u32) -> String {
    let peer = event
        .peer
        .as_ref()
        .map(|peer| peer.short_id())
        .unwrap_or_else(|| "unknown peer".to_string());
    let model = event.model.as_deref().unwrap_or("a model");

    let mut line = match event.kind {
        EventKind::PeerUp => format!("Peer {peer} joined the mesh"),
        EventKind::PeerDown => format!("Peer {peer} left the mesh"),
        EventKind::PeerUpdated => format!("Peer {peer} updated"),
        EventKind::NodeAccepting => "Node is accepting mesh connections".to_string(),
        EventKind::NodeStandby => "Node moved to standby".to_string(),
        EventKind::MeshIdUpdated => match event.mesh_id.as_deref() {
            Some(mesh_id) => format!("Mesh id is now {mesh_id}"),
            None => "Mesh id cleared".to_string(),
        },
        EventKind::ModelLoaded => format!("{model} is now served by peer {peer}"),
        EventKind::ModelUnloaded => format!("{model} is no longer served by peer {peer}"),
        EventKind::Test => "Test event from the event-webhook plugin".to_string(),
    };

    if suppressed > 0 {
        let plural = if suppressed == 1 { "" } else { "s" };
        line.push_str(&format!(
            " (+{suppressed} identical event{plural} coalesced)"
        ));
    }
    line
}

/// The name/value pairs shared by the Slack and Discord renderings. Empty
/// values are omitted rather than sent as blanks — Discord rejects an empty
/// field value outright.
fn detail_fields(delivery: &Delivery, max_list: usize) -> Vec<(String, String)> {
    let event = &delivery.event;
    let mut fields = vec![("Event".to_string(), event.kind.as_str().to_string())];

    if let Some(mesh_id) = &event.mesh_id {
        fields.push(("Mesh".to_string(), truncate_text(mesh_id, MAX_FIELD_CHARS)));
    }
    if let Some(model) = &event.model {
        fields.push(("Model".to_string(), truncate_text(model, MAX_FIELD_CHARS)));
    }
    if let Some(peer) = &event.peer {
        fields.push(("Peer".to_string(), peer.short_id()));
        if let Some(role) = &peer.role {
            fields.push(("Role".to_string(), truncate_text(role, MAX_FIELD_CHARS)));
        }
        if let Some(vram) = peer.vram_bytes {
            fields.push(("VRAM".to_string(), format_gib(vram)));
        }
        if let Some(rtt) = peer.rtt_ms {
            fields.push(("RTT".to_string(), format!("{rtt} ms")));
        }
        if !peer.serving_models.is_empty() {
            let listed = truncate_list(&peer.serving_models, max_list).join(", ");
            fields.push((
                "Serving".to_string(),
                truncate_text(&listed, MAX_FIELD_CHARS),
            ));
        }
    }
    if delivery.suppressed > 0 {
        fields.push(("Coalesced".to_string(), delivery.suppressed.to_string()));
    }
    fields
}

/// Caps a list and says how much was left out, instead of silently shortening.
pub fn truncate_list(items: &[String], max: usize) -> Vec<String> {
    let max = max.max(1);
    if items.len() <= max {
        return items.to_vec();
    }
    let mut out: Vec<String> = items[..max].to_vec();
    out.push(format!("+{} more", items.len() - max));
    out
}

/// Character-safe truncation: byte slicing would panic on a multi-byte model
/// name, and model names are user-supplied.
pub fn truncate_text(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

pub fn format_gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Good => "good",
        Severity::Notice => "notice",
        Severity::Warn => "warn",
    }
}

fn slack_color(severity: Severity) -> &'static str {
    match severity {
        Severity::Good => "#2eb886",
        Severity::Notice => "#3aa3e3",
        Severity::Warn => "#daa038",
    }
}

fn discord_color(severity: Severity) -> u32 {
    match severity {
        Severity::Good => 0x2E_CC71,
        Severity::Notice => 0x34_98DB,
        Severity::Warn => 0xE6_7E22,
    }
}

/// UTC timestamp as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
///
/// Hand-rolled rather than pulled in with a date crate: Discord needs ISO 8601
/// in the embed, and this is the entire requirement.
pub fn format_rfc3339_utc(timestamp_ms: u64) -> String {
    let total_secs = (timestamp_ms / 1_000) as i64;
    let millis = timestamp_ms % 1_000;
    let days = total_secs.div_euclid(86_400);
    let secs_of_day = total_secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to (y, m, d).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_position + 2) / 5 + 1) as u32;
    let month = (if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    }) as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::PeerSummary;

    fn peer() -> PeerSummary {
        PeerSummary {
            peer_id: "0123456789abcdef0123456789abcdef".to_string(),
            version: Some("0.72.1".to_string()),
            role: Some("host".to_string()),
            vram_bytes: Some(24 * 1024 * 1024 * 1024),
            rtt_ms: Some(12),
            serving_models: vec!["qwen3-8b".to_string(), "llama3-70b".to_string()],
            models: vec!["qwen3-8b".to_string()],
        }
    }

    fn delivery(kind: EventKind, suppressed: u32) -> Delivery {
        Delivery {
            event: NodeEvent {
                kind,
                node_id: "fedcba9876543210fedcba98".to_string(),
                mesh_id: Some("mesh-7".to_string()),
                peer: Some(peer()),
                model: (kind == EventKind::ModelLoaded || kind == EventKind::ModelUnloaded)
                    .then(|| "qwen3-8b".to_string()),
                detail: None,
                timestamp_ms: 1_700_000_000_000,
            },
            suppressed,
        }
    }

    #[test]
    fn the_generic_envelope_carries_every_field_a_receiver_needs() {
        let payload = render(PayloadFormat::Json, &delivery(EventKind::PeerUp, 0), 12);

        assert_eq!(payload["source"], json!("tdcc"));
        assert_eq!(payload["event"], json!("peer.up"));
        assert_eq!(payload["severity"], json!("good"));
        assert_eq!(payload["timestamp"], json!("2023-11-14T22:13:20.000Z"));
        assert_eq!(payload["coalesced"], json!(0));
        assert_eq!(payload["peer"]["short_id"], json!("0123456789ab…"));
    }

    #[test]
    fn slack_and_discord_payloads_have_the_shape_those_apis_require() {
        let slack = render(PayloadFormat::Slack, &delivery(EventKind::PeerDown, 0), 12);
        assert!(slack["text"].as_str().expect("text").starts_with("TDCC · "));
        assert_eq!(slack["attachments"][0]["color"], json!("#daa038"));
        assert!(
            slack["attachments"][0]["fields"]
                .as_array()
                .expect("fields")
                .len()
                >= 2
        );
        assert_eq!(slack["attachments"][0]["ts"], json!(1_700_000_000u64));

        let discord = render(PayloadFormat::Discord, &delivery(EventKind::PeerUp, 0), 12);
        let embed = &discord["embeds"][0];
        assert_eq!(embed["title"], json!("peer.up"));
        assert_eq!(embed["color"], json!(0x2E_CC71));
        assert_eq!(embed["timestamp"], json!("2023-11-14T22:13:20.000Z"));
        // Discord rejects an empty field value, so no field may be blank.
        for field in embed["fields"].as_array().expect("fields") {
            assert!(!field["value"].as_str().expect("value").is_empty());
        }
    }

    #[test]
    fn a_coalesced_run_is_stated_in_every_format() {
        let job = delivery(EventKind::PeerDown, 37);

        let generic = render(PayloadFormat::Json, &job, 12);
        assert_eq!(generic["coalesced"], json!(37));
        assert!(
            generic["summary"]
                .as_str()
                .expect("summary")
                .contains("+37 identical events coalesced")
        );

        let slack = render(PayloadFormat::Slack, &job, 12);
        assert!(slack["text"].as_str().expect("text").contains("+37"));

        let discord = render(PayloadFormat::Discord, &job, 12);
        assert!(
            discord["embeds"][0]["description"]
                .as_str()
                .expect("description")
                .contains("+37")
        );
    }

    #[test]
    fn one_suppressed_event_is_not_pluralised() {
        let summary = summary_line(&delivery(EventKind::PeerDown, 1).event, 1);
        assert!(
            summary.contains("+1 identical event coalesced"),
            "{summary}"
        );
    }

    #[test]
    fn model_events_name_the_model_and_the_peer() {
        let summary = summary_line(&delivery(EventKind::ModelUnloaded, 0).event, 0);
        assert_eq!(
            summary,
            "qwen3-8b is no longer served by peer 0123456789ab…"
        );
    }

    #[test]
    fn long_model_lists_are_elided_with_a_count_not_silently_cut() {
        let models: Vec<String> = (0..50).map(|index| format!("model-{index}")).collect();
        let truncated = truncate_list(&models, 3);

        assert_eq!(
            truncated,
            vec![
                "model-0".to_string(),
                "model-1".to_string(),
                "model-2".to_string(),
                "+47 more".to_string(),
            ]
        );
        assert_eq!(truncate_list(&models, 100).len(), 50);
    }

    #[test]
    fn a_peer_serving_hundreds_of_models_cannot_blow_up_any_payload() {
        let mut job = delivery(EventKind::PeerUpdated, 0);
        job.event.peer.as_mut().expect("peer").serving_models = (0..500)
            .map(|index| format!("some-fairly-long-model-name-{index}"))
            .collect();

        let discord = render(PayloadFormat::Discord, &job, 12).to_string();
        assert!(
            discord.len() < 6_000,
            "embed too large: {} bytes",
            discord.len()
        );

        // The generic envelope is capped too, with the same visible sentinel.
        let generic = render(PayloadFormat::Json, &job, 12);
        let serving = generic["peer"]["serving_models"]
            .as_array()
            .expect("serving_models");
        assert_eq!(serving.len(), 13);
        assert_eq!(serving[12], json!("+488 more"));
    }

    #[test]
    fn multibyte_text_truncation_does_not_split_a_character() {
        let text: String = "スカイ".repeat(500);
        let truncated = truncate_text(&text, 10);
        assert_eq!(truncated.chars().count(), 10);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn utc_formatting_matches_known_instants() {
        assert_eq!(format_rfc3339_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_rfc3339_utc(1_000_000_000_000),
            "2001-09-09T01:46:40.000Z"
        );
        assert_eq!(
            format_rfc3339_utc(1_700_000_000_123),
            "2023-11-14T22:13:20.123Z"
        );
        // 2020-02-29: a leap day, the classic off-by-one in hand-rolled dates.
        assert_eq!(
            format_rfc3339_utc(1_582_934_400_000),
            "2020-02-29T00:00:00.000Z"
        );
        assert_eq!(
            format_rfc3339_utc(1_583_020_799_000),
            "2020-02-29T23:59:59.000Z"
        );
    }

    #[test]
    fn vram_is_reported_in_binary_gigabytes() {
        assert_eq!(format_gib(24 * 1024 * 1024 * 1024), "24.0 GiB");
        assert_eq!(format_gib(0), "0.0 GiB");
    }
}
