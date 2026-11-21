//! Small helpers for walking document XML with quick-xml. Elements and
//! attributes are matched by local name, so the prefixes a producer chose do
//! not matter.

use quick_xml::XmlVersion;
use quick_xml::events::{BytesRef, BytesStart};
use quick_xml::name::QName;

use super::DocumentError;

/// The local name of an element, from its start or end tag.
pub(super) fn name(name: QName<'_>) -> &str {
    name.local_name().into_inner()
}

/// The value of the attribute with the local name `local`.
pub(super) fn attr(element: &BytesStart<'_>, local: &str) -> Option<String> {
    element
        .attributes()
        .with_checks(false)
        .flatten()
        .find(|attribute| attribute.key.local_name().into_inner() == local)
        .and_then(|attribute| {
            attribute
                .normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(Into::into)
        })
}

/// A numeric attribute, or `None` if it is missing or not a number.
pub(super) fn number_attr(element: &BytesStart<'_>, local: &str) -> Option<usize> {
    attr(element, local)?.trim().parse().ok()
}

/// The text an entity or character reference stands for.
pub(super) fn reference(reference: &BytesRef<'_>) -> Option<String> {
    if let Ok(Some(character)) = reference.resolve_char_ref() {
        return Some(character.to_string());
    }
    quick_xml::escape::resolve_predefined_entity(reference).map(str::to_owned)
}

/// Maps a parse error to what the user is told.
pub(super) fn damaged(error: &quick_xml::Error) -> DocumentError {
    tracing::debug!(%error, "malformed document XML");
    DocumentError::Damaged
}
