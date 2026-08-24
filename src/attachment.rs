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
        missing_effect: "This Entry Session cannot create Commission Proposals.",
    },
    Capability {
        name: COMMISSION_ACCEPTANCE,
        missing_effect: "This Entry Session cannot accept Commission Proposals.",
    },
    Capability {
        name: COMMISSION_INSPECTION,
        missing_effect: "This Entry Session cannot inspect a Commission.",
    },
    Capability {
        name: EVENT_REPLAY,
        missing_effect: "This Entry Session cannot replay unseen durable Commission events.",
    },
    Capability {
        name: CONTROL_TAKEOVER,
        missing_effect: "This Entry Session cannot take active control of a Commission.",
    },
    Capability {
        name: MATERIAL_NOTIFICATIONS,
        missing_effect: "This Entry Session must inspect for material Commission updates.",
    },
    Capability {
        name: PERSISTENT_MODE_DISPLAY,
        missing_effect: "Every Commission summary must repeat the Attachment Mode warning.",
    },
    Capability {
        name: WORKER_STEERING,
        missing_effect: "This Entry Session cannot steer an active Worker.",
    },
    Capability {
        name: WORKER_INTERRUPTION,
        missing_effect: "This Entry Session cannot interrupt an active Worker.",
    },
];

struct Capability {
    name: &'static str,
    missing_effect: &'static str,
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
        return Err(TyrionError::AttachmentRejected(
            "the adapter is incompatible because commission_inspection is required".into(),
        ));
    }

    let effective = CAPABILITIES
        .iter()
        .filter(|capability| advertised.contains(capability.name))
        .map(|capability| capability.name)
        .collect::<Vec<_>>();
    let missing = CAPABILITIES
        .iter()
        .filter(|capability| !advertised.contains(capability.name))
        .map(|capability| {
            json!({
                "capability": capability.name,
                "effect": capability.missing_effect,
                "alternative": "Use an Entry Session that advertises this capability.",
            })
        })
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
