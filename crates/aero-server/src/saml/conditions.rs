//! Signed SAML assertion usage-condition and identity validation.

use aero_common::Error as AeroError;
use time::{Duration, OffsetDateTime};
use uppsala::{Document, NodeId};

use super::{SamlAssertion, SamlConfig};

const PROTOCOL_NS: &str = "urn:oasis:names:tc:SAML:2.0:protocol";
const ASSERTION_NS: &str = "urn:oasis:names:tc:SAML:2.0:assertion";
const BEARER_METHOD: &str = "urn:oasis:names:tc:SAML:2.0:cm:bearer";
const SUCCESS_STATUS: &str = "urn:oasis:names:tc:SAML:2.0:status:Success";

/// Small, explicit allowance for ordinary SP/IdP clock drift.
pub(super) const CLOCK_SKEW_SECS: i64 = 90;
/// Assertions with excessively broad validity windows are rejected even if
/// their current instant happens to be valid.
const MAX_ASSERTION_WINDOW_SECS: i64 = 10 * 60;
const MAX_XML_BYTES: usize = 1024 * 1024;
const MAX_URI_BYTES: usize = 2048;
const MAX_NAME_ID_BYTES: usize = 512;
const MAX_ATTRIBUTE_NAME_BYTES: usize = 512;
const MAX_ATTRIBUTE_VALUE_BYTES: usize = 4096;
const MAX_ATTRIBUTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ValidatedResponse {
    pub request_id: String,
}

/// Validate the response shell and every signed Assertion condition used by the
/// SP. The XML signature and single-Assertion coverage must be verified before
/// this function is called.
pub(super) fn validate_response_conditions(
    cfg: &SamlConfig,
    xml: &str,
    now: OffsetDateTime,
) -> Result<ValidatedResponse, AeroError> {
    let doc = parse_bounded(xml)?;
    let response = response_root(&doc)?;
    let assertion = exactly_one_direct(&doc, response, ASSERTION_NS, "Assertion")?;

    let status = exactly_one_direct(&doc, response, PROTOCOL_NS, "Status")?;
    let status_code = exactly_one_direct(&doc, status, PROTOCOL_NS, "StatusCode")?;
    require_attr(
        &doc,
        status_code,
        "Value",
        MAX_URI_BYTES,
        "StatusCode Value",
    )
    .and_then(|value| require_equal(value, SUCCESS_STATUS, "response status"))?;

    let destination = require_attr(
        &doc,
        response,
        "Destination",
        MAX_URI_BYTES,
        "Response Destination",
    )?;
    require_equal(destination, &cfg.acs_url, "response Destination")?;
    let request_id = require_attr(
        &doc,
        response,
        "InResponseTo",
        super::request_state::MAX_REQUEST_ID_BYTES,
        "Response InResponseTo",
    )?;
    super::request_state::validate_request_id(request_id)?;

    let issuer = exactly_one_direct(&doc, assertion, ASSERTION_NS, "Issuer")?;
    let issuer = bounded_text(&doc, issuer, MAX_URI_BYTES, "Assertion Issuer")?;
    require_equal(issuer, &cfg.idp_entity_id, "assertion issuer")?;

    let conditions = exactly_one_direct(&doc, assertion, ASSERTION_NS, "Conditions")?;
    let not_before = parse_timestamp(require_attr(
        &doc,
        conditions,
        "NotBefore",
        64,
        "Conditions NotBefore",
    )?)?;
    let not_on_or_after = parse_timestamp(require_attr(
        &doc,
        conditions,
        "NotOnOrAfter",
        64,
        "Conditions NotOnOrAfter",
    )?)?;
    validate_window(now, not_before, not_on_or_after)?;
    validate_condition_elements(&doc, conditions)?;
    validate_audiences(&doc, conditions, &cfg.sp_entity_id)?;

    let subject = exactly_one_direct(&doc, assertion, ASSERTION_NS, "Subject")?;
    let confirmation = exactly_one_direct(&doc, subject, ASSERTION_NS, "SubjectConfirmation")?;
    let method = require_attr(
        &doc,
        confirmation,
        "Method",
        MAX_URI_BYTES,
        "SubjectConfirmation Method",
    )?;
    require_equal(method, BEARER_METHOD, "subject confirmation method")?;
    let confirmation_data =
        exactly_one_direct(&doc, confirmation, ASSERTION_NS, "SubjectConfirmationData")?;
    let recipient = require_attr(
        &doc,
        confirmation_data,
        "Recipient",
        MAX_URI_BYTES,
        "SubjectConfirmationData Recipient",
    )?;
    require_equal(recipient, &cfg.acs_url, "subject Recipient")?;
    let subject_request_id = require_attr(
        &doc,
        confirmation_data,
        "InResponseTo",
        super::request_state::MAX_REQUEST_ID_BYTES,
        "SubjectConfirmationData InResponseTo",
    )?;
    require_equal(
        subject_request_id,
        request_id,
        "response/subject InResponseTo",
    )?;
    let subject_expiry = parse_timestamp(require_attr(
        &doc,
        confirmation_data,
        "NotOnOrAfter",
        64,
        "SubjectConfirmationData NotOnOrAfter",
    )?)?;
    if let Some(raw_subject_not_before) = doc.get_attribute(confirmation_data, "NotBefore") {
        let raw_subject_not_before = raw_subject_not_before.trim();
        if raw_subject_not_before.is_empty() {
            return Err(unauthorized(
                "SubjectConfirmationData NotBefore must not be empty",
            ));
        }
        let subject_not_before = parse_timestamp(raw_subject_not_before)?;
        if now
            .checked_add(Duration::seconds(CLOCK_SKEW_SECS))
            .ok_or_else(|| unauthorized("clock overflow"))?
            < subject_not_before
        {
            return Err(unauthorized("subject confirmation is not yet valid"));
        }
        if subject_not_before >= subject_expiry {
            return Err(unauthorized(
                "subject confirmation validity window is empty",
            ));
        }
    }
    if now
        .checked_sub(Duration::seconds(CLOCK_SKEW_SECS))
        .ok_or_else(|| unauthorized("clock underflow"))?
        >= subject_expiry
    {
        return Err(unauthorized("subject confirmation has expired"));
    }

    Ok(ValidatedResponse {
        request_id: request_id.to_owned(),
    })
}

/// Extract identity only from the one signed Assertion, never from unsigned
/// lookalike elements in the Response shell.
pub(super) fn extract_signed_identity(xml: &str) -> Result<SamlAssertion, AeroError> {
    let doc = parse_bounded(xml)?;
    let response = response_root(&doc)?;
    let assertion = exactly_one_direct(&doc, response, ASSERTION_NS, "Assertion")?;
    let issuer_node = exactly_one_direct(&doc, assertion, ASSERTION_NS, "Issuer")?;
    let issuer = bounded_text(&doc, issuer_node, MAX_URI_BYTES, "Assertion Issuer")?;
    let subject = exactly_one_direct(&doc, assertion, ASSERTION_NS, "Subject")?;
    let name_id_node = exactly_one_direct(&doc, subject, ASSERTION_NS, "NameID")?;
    let name_id = bounded_text(&doc, name_id_node, MAX_NAME_ID_BYTES, "NameID")?;

    let mut attributes = Vec::new();
    for statement in doc.child_elements_by_name_ns(assertion, ASSERTION_NS, "AttributeStatement") {
        for attribute in doc.child_elements_by_name_ns(statement, ASSERTION_NS, "Attribute") {
            if attributes.len() >= MAX_ATTRIBUTES {
                return Err(unauthorized("too many assertion attributes"));
            }
            let name = require_attr(
                &doc,
                attribute,
                "Name",
                MAX_ATTRIBUTE_NAME_BYTES,
                "Attribute Name",
            )?;
            let values = doc.child_elements_by_name_ns(attribute, ASSERTION_NS, "AttributeValue");
            if let Some(value_node) = values.first() {
                let value = bounded_text(
                    &doc,
                    *value_node,
                    MAX_ATTRIBUTE_VALUE_BYTES,
                    "AttributeValue",
                )?;
                attributes.push((name.to_owned(), value.to_owned()));
            }
        }
    }

    Ok(SamlAssertion {
        issuer: issuer.to_owned(),
        name_id: name_id.to_owned(),
        attributes,
    })
}

fn parse_bounded(xml: &str) -> Result<Document<'_>, AeroError> {
    if xml.len() > MAX_XML_BYTES {
        return Err(unauthorized("decoded response exceeds 1 MiB"));
    }
    uppsala::parse(xml).map_err(|error| unauthorized(&format!("invalid XML: {error}")))
}

fn response_root(doc: &Document<'_>) -> Result<NodeId, AeroError> {
    let root = doc
        .document_element()
        .ok_or_else(|| unauthorized("response has no document element"))?;
    let is_response = doc
        .element(root)
        .is_some_and(|element| element.matches_name_ns(PROTOCOL_NS, "Response"));
    if !is_response {
        return Err(unauthorized(
            "document element is not a SAML protocol Response",
        ));
    }
    Ok(root)
}

fn exactly_one_direct(
    doc: &Document<'_>,
    parent: NodeId,
    namespace: &str,
    local_name: &str,
) -> Result<NodeId, AeroError> {
    let nodes = doc.child_elements_by_name_ns(parent, namespace, local_name);
    match nodes.as_slice() {
        [node] => Ok(*node),
        _ => Err(unauthorized(&format!(
            "expected exactly one {local_name}, found {}",
            nodes.len()
        ))),
    }
}

fn require_attr<'a>(
    doc: &'a Document<'_>,
    node: NodeId,
    attribute: &str,
    max_bytes: usize,
    label: &str,
) -> Result<&'a str, AeroError> {
    let value = doc
        .get_attribute(node, attribute)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| unauthorized(&format!("{label} is required")))?;
    if value.len() > max_bytes {
        return Err(unauthorized(&format!("{label} is too long")));
    }
    Ok(value)
}

fn bounded_text<'a>(
    doc: &'a Document<'_>,
    node: NodeId,
    max_bytes: usize,
    label: &str,
) -> Result<&'a str, AeroError> {
    let value = doc
        .element_text(node)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| unauthorized(&format!("{label} is required")))?;
    if value.len() > max_bytes {
        return Err(unauthorized(&format!("{label} is too long")));
    }
    Ok(value)
}

fn require_equal(actual: &str, expected: &str, label: &str) -> Result<(), AeroError> {
    if actual == expected {
        Ok(())
    } else {
        Err(unauthorized(&format!(
            "{label} does not match SP configuration"
        )))
    }
}

fn parse_timestamp(value: &str) -> Result<OffsetDateTime, AeroError> {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map_err(|_| unauthorized("invalid SAML timestamp"))
}

fn validate_window(
    now: OffsetDateTime,
    not_before: OffsetDateTime,
    not_on_or_after: OffsetDateTime,
) -> Result<(), AeroError> {
    if not_on_or_after <= not_before {
        return Err(unauthorized("assertion validity window is empty"));
    }
    if not_on_or_after - not_before > Duration::seconds(MAX_ASSERTION_WINDOW_SECS) {
        return Err(unauthorized("assertion validity window exceeds 10 minutes"));
    }
    if now
        .checked_add(Duration::seconds(CLOCK_SKEW_SECS))
        .ok_or_else(|| unauthorized("clock overflow"))?
        < not_before
    {
        return Err(unauthorized("assertion is not yet valid"));
    }
    if now
        .checked_sub(Duration::seconds(CLOCK_SKEW_SECS))
        .ok_or_else(|| unauthorized("clock underflow"))?
        >= not_on_or_after
    {
        return Err(unauthorized("assertion has expired"));
    }
    Ok(())
}

fn validate_condition_elements(doc: &Document<'_>, conditions: NodeId) -> Result<(), AeroError> {
    let mut one_time_use = 0usize;
    for node in doc.children_iter(conditions) {
        let Some(element) = doc.element(node) else {
            continue;
        };
        if element.matches_name_ns(ASSERTION_NS, "AudienceRestriction") {
            continue;
        }
        if element.matches_name_ns(ASSERTION_NS, "OneTimeUse") {
            one_time_use += 1;
            if one_time_use > 1
                || doc
                    .children_iter(node)
                    .any(|child| doc.element(child).is_some())
            {
                return Err(unauthorized("invalid OneTimeUse condition"));
            }
            continue;
        }
        return Err(unauthorized(
            "unsupported signed Assertion condition (fail-closed)",
        ));
    }
    Ok(())
}

fn validate_audiences(
    doc: &Document<'_>,
    conditions: NodeId,
    expected: &str,
) -> Result<(), AeroError> {
    let restrictions =
        doc.child_elements_by_name_ns(conditions, ASSERTION_NS, "AudienceRestriction");
    if restrictions.is_empty() {
        return Err(unauthorized("AudienceRestriction is required"));
    }
    for restriction in restrictions {
        let audiences = doc.child_elements_by_name_ns(restriction, ASSERTION_NS, "Audience");
        let matches = audiences.into_iter().any(|node| {
            doc.element_text(node)
                .map(str::trim)
                .is_some_and(|audience| {
                    !audience.is_empty() && audience.len() <= MAX_URI_BYTES && audience == expected
                })
        });
        if !matches {
            return Err(unauthorized("assertion audience does not include this SP"));
        }
    }
    Ok(())
}

fn unauthorized(message: &str) -> AeroError {
    AeroError::Unauthorized(format!("saml: {message}"))
}
