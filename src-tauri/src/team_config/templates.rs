// Built-in team templates. JSON sources are owned by sidecar/ — we embed
// them at compile time via include_str! so there is exactly one copy.

use std::collections::HashMap;

use chrono::Utc;
use uuid::Uuid;

use super::types::TeamConfig;

const SOLO_PM_JSON: &str =
    include_str!("../../../../sidecar/src/team/templates/solo_pm.json");
const PM_PLUS_ONE_DEV_JSON: &str =
    include_str!("../../../../sidecar/src/team/templates/pm_plus_one_dev.json");
const PM_TWO_DEV_QA_JSON: &str =
    include_str!("../../../../sidecar/src/team/templates/pm_two_dev_qa.json");

/// Display metadata for the three built-in templates. Order is the order
/// shown in the picker. Consumed by `list_templates`.
pub(crate) const TEMPLATE_METADATA: &[(&str, &str, &str)] = &[
    ("solo_pm", "Solo PM", "Just a PM. Closest to a single-agent chat experience."),
    (
        "pm_plus_one_dev",
        "PM + 1 Dev",
        "Minimal hierarchy. The default starter team.",
    ),
    (
        "pm_two_dev_qa",
        "PM + 2 Dev + 1 QA",
        "Canonical demo team — parallel devs plus a QA reviewer.",
    ),
];

fn lookup_template_json(template_id: &str) -> Option<&'static str> {
    match template_id {
        "solo_pm" => Some(SOLO_PM_JSON),
        "pm_plus_one_dev" => Some(PM_PLUS_ONE_DEV_JSON),
        "pm_two_dev_qa" => Some(PM_TWO_DEV_QA_JSON),
        _ => None,
    }
}

pub(crate) fn instantiate_template(template_id: &str) -> Result<TeamConfig, String> {
    let json = lookup_template_json(template_id)
        .ok_or_else(|| format!("unknown template id: {template_id}"))?;

    let mut config: TeamConfig = serde_json::from_str(json)
        .map_err(|err| format!("template {template_id} failed to parse: {err}"))?;

    let mut id_map: HashMap<String, String> = HashMap::with_capacity(config.agents.len());
    for agent in &mut config.agents {
        let new_id = format!("agent_{}", Uuid::new_v4());
        id_map.insert(agent.id.clone(), new_id.clone());
        agent.id = new_id;
    }

    for sub in &mut config.subscriptions {
        sub.publisher = id_map.get(&sub.publisher).cloned().ok_or_else(|| {
            format!(
                "template {template_id}: subscription publisher references unknown agent id {}",
                sub.publisher
            )
        })?;
        let mut new_subscribers = Vec::with_capacity(sub.subscribers.len());
        for old in &sub.subscribers {
            let new_id = id_map.get(old).cloned().ok_or_else(|| {
                format!(
                    "template {template_id}: subscription subscriber references unknown agent id {old}"
                )
            })?;
            new_subscribers.push(new_id);
        }
        sub.subscribers = new_subscribers;
    }

    config.id = format!("team_{}", Uuid::new_v4());
    config.created_at = Utc::now().to_rfc3339();

    Ok(config)
}
