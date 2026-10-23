//! Media in the transcript (PLAN section 5): how an attachment is shown to the model.

use giles_types::MediaRef;

/// The text that names an attachment for the model: `[media:<id> <name> <type>]`, or
/// `[media:<id> <type>]` without a name (PLAN section 5). It stands in for media the model
/// doesn't receive as content.
pub fn reference(media: &MediaRef) -> String {
    match &media.name {
        Some(name) => format!("[media:{} {name} {}]", media.id, media.media_type),
        None => format!("[media:{} {}]", media.id, media.media_type),
    }
}

#[cfg(test)]
mod tests {
    use giles_types::MediaId;

    use super::*;

    #[test]
    fn references_name_the_id_the_name_and_the_type() {
        let mut media = MediaRef {
            id: MediaId::from_digest([1; 32]),
            media_type: "text/csv".into(),
            size: 1,
            name: None,
        };
        assert_eq!(reference(&media), format!("[media:{} text/csv]", media.id));
        media.name = Some("q3.csv".into());
        assert_eq!(
            reference(&media),
            format!("[media:{} q3.csv text/csv]", media.id)
        );
    }
}
