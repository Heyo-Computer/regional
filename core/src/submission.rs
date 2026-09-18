//! Public submissions: requests to add, change or remove indexed content.
//!
//! Lives in `core` because two services touch it — the dashboard writes and
//! reviews submissions, and the indexer consumes the approved ones — and a
//! disagreement about the shape between those two would be silent.

use serde::{Deserialize, Serialize};

/// What the submitter is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Request {
    /// Something missing from the index.
    Add,
    /// Something indexed whose details have changed.
    Update,
    /// Something indexed that should not be (closed, demolished, duplicate).
    Remove,
    /// Something indexed with a specific factual error.
    Correction,
}

impl Request {
    pub const ALL: [Request; 4] = [
        Request::Add,
        Request::Update,
        Request::Remove,
        Request::Correction,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Request::Add => "add",
            Request::Update => "update",
            Request::Remove => "remove",
            Request::Correction => "correction",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Request::Add => "Add something new",
            Request::Update => "Update something that changed",
            Request::Remove => "Remove something",
            Request::Correction => "Correct a mistake",
        }
    }

    pub fn parse(s: &str) -> Option<Request> {
        match s.trim().to_ascii_lowercase().as_str() {
            "add" => Some(Request::Add),
            "update" => Some(Request::Update),
            "remove" => Some(Request::Remove),
            "correction" => Some(Request::Correction),
            _ => None,
        }
    }

    /// Does this request refer to something already in the index?
    pub fn needs_existing(&self) -> bool {
        matches!(
            self,
            Request::Update | Request::Remove | Request::Correction
        )
    }
}

impl std::fmt::Display for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a submission is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Waiting for a human.
    Pending,
    /// Accepted; waiting for the indexer to act on it.
    Approved,
    /// Accepted and acted on. Terminal.
    Applied,
    /// Declined. Terminal.
    Rejected,
}

impl Status {
    pub const ALL: [Status; 4] = [
        Status::Pending,
        Status::Approved,
        Status::Applied,
        Status::Rejected,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Approved => "approved",
            Status::Applied => "applied",
            Status::Rejected => "rejected",
        }
    }

    pub fn parse(s: &str) -> Option<Status> {
        Status::ALL
            .into_iter()
            .find(|v| v.as_str() == s.trim().to_ascii_lowercase())
    }

    /// Wording for the submitter, who sees this on the public status page.
    pub fn public_label(&self) -> &'static str {
        match self {
            Status::Pending => "Waiting for review",
            Status::Approved => "Approved — queued for the next index run",
            Status::Applied => "Done — this is live in the index",
            Status::Rejected => "Not accepted",
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One request, as stored.
///
/// The `id` doubles as the submitter's reference: it is unguessable, so the
/// public status page needs no account behind it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Submission {
    pub id: String,
    pub status: Status,
    pub request: Request,
    /// Which index this concerns: `place`, `event` or `article`.
    pub target_kind: String,

    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lng: Option<f64>,
    #[serde(default)]
    pub categories: Vec<String>,
    /// For update/remove/correction: the `id` of the existing document.
    #[serde(default)]
    pub existing_id: Option<String>,
    /// Optional, and never shown on any public page.
    #[serde(default)]
    pub contact: Option<String>,

    pub submitted_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub reviewed_at: Option<i64>,
    /// The reviewer's note. Shown to the submitter, so it is written for them.
    #[serde(default)]
    pub review_note: Option<String>,
    /// Why the indexer could not act on an approved submission, if it could not.
    #[serde(default)]
    pub apply_note: Option<String>,
}

impl Submission {
    /// Does this carry enough to place it on the map without a geocoder?
    pub fn has_coords(&self) -> bool {
        matches!((self.lat, self.lng), (Some(lat), Some(lng))
            if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lng))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_through_their_wire_form() {
        for r in Request::ALL {
            assert_eq!(Request::parse(r.as_str()), Some(r));
            assert_eq!(Request::parse(&r.as_str().to_uppercase()), Some(r));
        }
        assert_eq!(Request::parse("delete"), None);
    }

    #[test]
    fn statuses_round_trip_and_serialise_lowercase() {
        for s in Status::ALL {
            assert_eq!(Status::parse(s.as_str()), Some(s));
            assert_eq!(
                serde_json::to_value(s).unwrap(),
                serde_json::Value::String(s.as_str().into())
            );
        }
        assert_eq!(Status::parse("nope"), None);
    }

    #[test]
    fn only_edits_need_an_existing_document() {
        assert!(!Request::Add.needs_existing());
        assert!(Request::Update.needs_existing());
        assert!(Request::Remove.needs_existing());
        assert!(Request::Correction.needs_existing());
    }

    #[test]
    fn coordinate_validity_is_checked_not_assumed() {
        let base = Submission {
            id: "x".into(),
            status: Status::Pending,
            request: Request::Add,
            target_kind: "place".into(),
            title: "T".into(),
            description: String::new(),
            url: None,
            address: None,
            city: None,
            lat: None,
            lng: None,
            categories: vec![],
            existing_id: None,
            contact: None,
            submitted_at: 0,
            updated_at: 0,
            reviewed_at: None,
            review_note: None,
            apply_note: None,
        };
        assert!(!base.has_coords());
        let good = Submission {
            lat: Some(39.1),
            lng: Some(-108.3),
            ..base.clone()
        };
        assert!(good.has_coords());
        let half = Submission {
            lat: Some(39.1),
            ..base.clone()
        };
        assert!(!half.has_coords(), "one coordinate is not a location");
        let silly = Submission {
            lat: Some(999.0),
            lng: Some(-108.3),
            ..base
        };
        assert!(!silly.has_coords());
    }
}
