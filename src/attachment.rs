use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::TyrionError;

pub(crate) const PROPOSAL_CREATION: &str = "proposal_creation";
pub(crate) const COMMISSION_ACCEPTANCE: &str = "commission_acceptance";
pub(crate) const COMMISSION_INSPECTION: &str = "commission_inspection";
pub(crate) const EVENT_REPLAY: &str = "event_replay";
pub(crate) const CONTROL_TAKEOVER: &str = "control_takeover";
pub(crate) const MATERIAL_NOTIFICATIONS: &str = "material_notifications";
pub(crate) const PERSISTENT_MODE_DISPLAY: &str = "persistent_mode_display";
pub(crate) const WORKER_STEERING: &str = "worker_steering";
pub(crate) const WORKER_INTERRUPTION: &str = "worker_interruption";

const CAPABILITIES: [Capability; 9] = [
    Capability {
        name: PROPOSAL_CREATION,
        affected_actions: &["create_proposal"],
        missing_effect: "This Entry Session cannot create Commission Proposals.",
    },
    Capability {
        name: COMMISSION_ACCEPTANCE,
        affected_actions: &[
            "accept_commission",
            "pause_commission",
            "resume_commission",
            "cancel_commission",
            "propose_operation",
            "execute_operation",
            "propose_commission_amendment",
            "amend_verification",
            "record_verification_evidence",
        ],
        missing_effect: "This Entry Session cannot accept Commission Proposals.",
    },
    Capability {
        name: COMMISSION_INSPECTION,
        affected_actions: &[
            "connect_attachment",
            "inspect_commission",
            "update_attachment_capabilities",
            "create_profile_claim",
            "revise_profile_claim",
            "observe_profile_preference",
            "confirm_profile_claim",
            "suppress_profile_claim",
            "forget_profile_claim",
            "create_learning_boundary",
            "import_memory",
            "pin_memory_material",
        ],
        missing_effect: "This Entry Session cannot inspect a Commission.",
    },
    Capability {
        name: EVENT_REPLAY,
        affected_actions: &[
            "connect_attachment_with_replay",
            "resume_attachment",
            "replay_events",
        ],
        missing_effect: "This Entry Session cannot replay unseen durable Commission events.",
    },
    Capability {
        name: CONTROL_TAKEOVER,
        affected_actions: &["take_control"],
        missing_effect: "This Entry Session cannot take active control of a Commission.",
    },
    Capability {
        name: MATERIAL_NOTIFICATIONS,
        affected_actions: &["receive_material_notifications"],
        missing_effect: "This Entry Session must inspect for material Commission updates.",
    },
    Capability {
        name: PERSISTENT_MODE_DISPLAY,
        affected_actions: &["display_attachment_mode_persistently"],
        missing_effect: "Every Commission summary must repeat the Attachment Mode warning.",
    },
    Capability {
        name: WORKER_STEERING,
        affected_actions: &["steer_worker"],
        missing_effect: "This Entry Session cannot steer an active Worker.",
    },
    Capability {
        name: WORKER_INTERRUPTION,
        affected_actions: &["interrupt_worker", "retry_worker"],
        missing_effect: "This Entry Session cannot interrupt an active Worker.",
    },
];

struct Capability {
    name: &'static str,
    affected_actions: &'static [&'static str],
    missing_effect: &'static str,
}

const FULL_CONTROL_HARNESS: &str = "pi";

fn missing_capability(capability: &Capability) -> Value {
    json!({
        "capability": capability.name,
        "affected_actions": capability.affected_actions,
        "effect": capability.missing_effect,
        "alternative": "Reconnect through a Full Pi Entry Session.",
        "supported_harness": FULL_CONTROL_HARNESS,
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttachmentMode {
    Full,
    Limited,
    Observer,
}

impl AttachmentMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Limited => "limited",
            Self::Observer => "observer",
        }
    }

    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Full => "Tyrion: Full",
            Self::Limited => "Tyrion: Limited",
            Self::Observer => "Tyrion: Observer",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, TyrionError> {
        match value {
            "full" => Ok(Self::Full),
            "limited" => Ok(Self::Limited),
            "observer" => Ok(Self::Observer),
            _ => Err(TyrionError::InvalidRequest(format!(
                "stored Attachment Mode {value} is invalid"
            ))),
        }
    }
}

pub(crate) struct NegotiatedCapabilities {
    pub(crate) effective: Vec<&'static str>,
    pub(crate) missing: Vec<Value>,
    pub(crate) mode: AttachmentMode,
}

pub(crate) fn negotiate(advertised: &[String]) -> Result<NegotiatedCapabilities, TyrionError> {
    let advertised = advertised
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    if !advertised.contains(COMMISSION_INSPECTION) {
        let inspection = CAPABILITIES
            .iter()
            .find(|capability| capability.name == COMMISSION_INSPECTION)
            .expect("commission_inspection must remain a known capability");
        return Err(TyrionError::AttachmentRejectedWithDetails {
            message: "the adapter is incompatible because commission_inspection is required".into(),
            details: json!({"missing_capabilities": [missing_capability(inspection)]}),
        });
    }

    let effective = CAPABILITIES
        .iter()
        .filter(|capability| advertised.contains(capability.name))
        .map(|capability| capability.name)
        .collect::<Vec<_>>();
    let missing = CAPABILITIES
        .iter()
        .filter(|capability| !advertised.contains(capability.name))
        .map(missing_capability)
        .collect::<Vec<_>>();
    let can_mutate = advertised.contains(PROPOSAL_CREATION)
        || advertised.contains(COMMISSION_ACCEPTANCE)
        || advertised.contains(CONTROL_TAKEOVER);
    let mode = if !can_mutate {
        AttachmentMode::Observer
    } else if missing.is_empty() {
        AttachmentMode::Full
    } else {
        AttachmentMode::Limited
    };

    Ok(NegotiatedCapabilities {
        effective,
        missing,
        mode,
    })
}
