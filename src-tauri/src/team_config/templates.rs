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
        // The literal "user" pseudo-publisher (USER_PUBLISHER) is identity-
        // stable across template instantiations — only real agent ids get
        // freshly minted UUIDs. Mirrors sidecar's `instantiateTemplate`.
        if sub.publisher != crate::team_config::types::USER_PUBLISHER {
            sub.publisher = id_map.get(&sub.publisher).cloned().ok_or_else(|| {
                format!(
                    "template {template_id}: subscription publisher references unknown agent id {}",
                    sub.publisher
                )
            })?;
        }
        let mut new_subscribers = Vec::with_capacity(sub.subscribers.len());
        for old in &sub.subscribers {
            if old == crate::team_config::types::USER_PUBLISHER {
                new_subscribers.push(old.clone());
                continue;
            }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use std::collections::HashSet;

    #[test]
    fn test_lookup_template_known() {
        for id in &["solo_pm", "pm_plus_one_dev", "pm_two_dev_qa"] {
            let json = lookup_template_json(id);
            assert!(json.is_some(), "missing template {id}");
            // Smoke-check that the embedded JSON parses as TeamConfig.
            let _parsed: TeamConfig = serde_json::from_str(json.unwrap())
                .unwrap_or_else(|e| panic!("template {id} unparseable: {e}"));
        }
    }

    #[test]
    fn test_lookup_template_unknown() {
        assert!(lookup_template_json("does_not_exist").is_none());
        assert!(instantiate_template("does_not_exist").is_err());
    }

    #[test]
    fn test_instantiate_template_fresh_ids() {
        let t1 = instantiate_template("solo_pm").unwrap();
        let t2 = instantiate_template("solo_pm").unwrap();

        assert_ne!(t1.id, t2.id);
        assert!(t1.id.starts_with("team_"));
        assert!(t2.id.starts_with("team_"));

        assert_eq!(t1.agents.len(), 1);
        assert_eq!(t2.agents.len(), 1);
        assert_ne!(t1.agents[0].id, t2.agents[0].id);
        assert!(t1.agents[0].id.starts_with("agent_"));
    }

    #[test]
    fn test_instantiate_template_reference_remap() {
        let team = instantiate_template("pm_two_dev_qa").unwrap();

        let agent_ids: HashSet<&str> =
            team.agents.iter().map(|a| a.id.as_str()).collect();

        // No placeholder ids should leak through.
        for id in &agent_ids {
            assert!(
                !id.starts_with("tmpl_"),
                "placeholder id leaked into instance: {id}"
            );
        }

        // Every subscription publisher + subscriber must resolve to an
        // instantiated agent id.
        assert!(!team.subscriptions.is_empty());
        for sub in &team.subscriptions {
            assert!(
                agent_ids.contains(sub.publisher.as_str()),
                "publisher {} not in agent ids",
                sub.publisher
            );
            for s in &sub.subscribers {
                assert!(
                    agent_ids.contains(s.as_str()),
                    "subscriber {s} not in agent ids"
                );
            }
        }
    }

    #[test]
    fn test_instantiate_template_created_at() {
        let team = instantiate_template("pm_plus_one_dev").unwrap();
        DateTime::parse_from_rfc3339(&team.created_at).unwrap_or_else(|e| {
            panic!("created_at not RFC3339: {} ({e})", team.created_at)
        });
    }
}
