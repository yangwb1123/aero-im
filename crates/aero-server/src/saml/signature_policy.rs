//! Strict SAML XML-DSig profile preflight for the experimental verifier.
//!
//! `bergshamra` implements a broad XML-DSig surface, including local-file
//! reference resolution and expensive XPath/XSLT transforms. SAML Web Browser
//! SSO needs only one narrow enveloped-signature profile. Reject everything
//! outside that profile before untrusted XML reaches the verifier.

use aero_common::Error as AeroError;
use uppsala::{Document, NodeId};

const PROTOCOL_NS: &str = "urn:oasis:names:tc:SAML:2.0:protocol";
const ASSERTION_NS: &str = "urn:oasis:names:tc:SAML:2.0:assertion";
const DSIG_NS: &str = "http://www.w3.org/2000/09/xmldsig#";
const EXCLUSIVE_C14N: &str = "http://www.w3.org/2001/10/xml-exc-c14n#";
const ENVELOPED_SIGNATURE: &str = "http://www.w3.org/2000/09/xmldsig#enveloped-signature";
const RSA_SHA256: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256";
const SHA256: &str = "http://www.w3.org/2001/04/xmlenc#sha256";
const MAX_NODES: usize = 4096;
const MAX_ASSERTION_ID_BYTES: usize = 256;

pub(super) fn validate_signature_profile(xml: &str) -> Result<(), AeroError> {
    let doc = uppsala::parse(xml).map_err(|error| {
        unauthorized(&format!(
            "invalid XML before signature verification: {error}"
        ))
    })?;
    let root = doc
        .document_element()
        .ok_or_else(|| unauthorized("signature response has no document element"))?;
    if !matches_element(&doc, root, PROTOCOL_NS, "Response") {
        return Err(unauthorized(
            "signature document element is not a SAML Response",
        ));
    }
    if doc.descendants(root).len() > MAX_NODES {
        return Err(unauthorized("signature document has too many nodes"));
    }

    let assertion = exactly_one_global(&doc, ASSERTION_NS, "Assertion")?;
    if doc.parent(assertion) != Some(root) {
        return Err(unauthorized(
            "signed Assertion must be a direct Response child",
        ));
    }
    let assertion_id = required_attribute(&doc, assertion, "ID", "Assertion ID")?;
    if assertion_id.len() > MAX_ASSERTION_ID_BYTES {
        return Err(unauthorized("Assertion ID is too long"));
    }

    let signature = exactly_one_global(&doc, DSIG_NS, "Signature")?;
    if doc.parent(signature) != Some(assertion) {
        return Err(unauthorized(
            "Signature must be a direct child of the consumed Assertion",
        ));
    }
    let signed_info = exactly_one_direct(&doc, signature, DSIG_NS, "SignedInfo")?;
    let signature_value = exactly_one_direct(&doc, signature, DSIG_NS, "SignatureValue")?;
    require_only_element_children(
        &doc,
        signature,
        &[signed_info, signature_value],
        "Signature",
    )?;

    let canonicalization =
        exactly_one_direct(&doc, signed_info, DSIG_NS, "CanonicalizationMethod")?;
    require_algorithm(&doc, canonicalization, EXCLUSIVE_C14N, "canonicalization")?;
    require_no_element_children(&doc, canonicalization, "CanonicalizationMethod")?;

    let signature_method = exactly_one_direct(&doc, signed_info, DSIG_NS, "SignatureMethod")?;
    require_algorithm(&doc, signature_method, RSA_SHA256, "signature")?;
    require_no_element_children(&doc, signature_method, "SignatureMethod")?;

    let reference = exactly_one_direct(&doc, signed_info, DSIG_NS, "Reference")?;
    if exactly_one_global(&doc, DSIG_NS, "Reference")? != reference {
        return Err(unauthorized("ambiguous signature Reference"));
    }
    let expected_uri = format!("#{assertion_id}");
    let uri = required_attribute(&doc, reference, "URI", "Reference URI")?;
    if uri != expected_uri {
        return Err(unauthorized(
            "Reference URI must be the consumed Assertion's same-document ID",
        ));
    }
    require_only_element_children(
        &doc,
        signed_info,
        &[canonicalization, signature_method, reference],
        "SignedInfo",
    )?;

    let transforms = exactly_one_direct(&doc, reference, DSIG_NS, "Transforms")?;
    let transform_nodes = doc.child_elements_by_name_ns(transforms, DSIG_NS, "Transform");
    let [enveloped, exclusive] = transform_nodes.as_slice() else {
        return Err(unauthorized(
            "Reference must use exactly two approved transforms",
        ));
    };
    require_algorithm(&doc, *enveloped, ENVELOPED_SIGNATURE, "first transform")?;
    require_algorithm(&doc, *exclusive, EXCLUSIVE_C14N, "second transform")?;
    require_no_element_children(&doc, *enveloped, "enveloped transform")?;
    require_no_element_children(&doc, *exclusive, "canonicalization transform")?;
    require_only_element_children(&doc, transforms, &transform_nodes, "Transforms")?;

    let digest_method = exactly_one_direct(&doc, reference, DSIG_NS, "DigestMethod")?;
    require_algorithm(&doc, digest_method, SHA256, "digest")?;
    require_no_element_children(&doc, digest_method, "DigestMethod")?;
    let digest_value = exactly_one_direct(&doc, reference, DSIG_NS, "DigestValue")?;
    require_only_element_children(
        &doc,
        reference,
        &[transforms, digest_method, digest_value],
        "Reference",
    )?;

    // Reject duplicate or displaced core nodes, even if the verifier would
    // happen to select only the first one.
    for (name, expected) in [
        ("SignedInfo", signed_info),
        ("CanonicalizationMethod", canonicalization),
        ("SignatureMethod", signature_method),
        ("Transforms", transforms),
        ("DigestMethod", digest_method),
        ("DigestValue", digest_value),
        ("SignatureValue", signature_value),
    ] {
        if exactly_one_global(&doc, DSIG_NS, name)? != expected {
            return Err(unauthorized(&format!("ambiguous {name}")));
        }
    }
    Ok(())
}

fn exactly_one_global(
    doc: &Document<'_>,
    namespace: &str,
    name: &str,
) -> Result<NodeId, AeroError> {
    let nodes = doc.get_elements_by_tag_name_ns(namespace, name);
    match nodes.as_slice() {
        [node] => Ok(*node),
        _ => Err(unauthorized(&format!(
            "expected exactly one {name}, found {}",
            nodes.len()
        ))),
    }
}

fn exactly_one_direct(
    doc: &Document<'_>,
    parent: NodeId,
    namespace: &str,
    name: &str,
) -> Result<NodeId, AeroError> {
    let nodes = doc.child_elements_by_name_ns(parent, namespace, name);
    match nodes.as_slice() {
        [node] => Ok(*node),
        _ => Err(unauthorized(&format!(
            "expected exactly one direct {name}, found {}",
            nodes.len()
        ))),
    }
}

fn required_attribute<'a>(
    doc: &'a Document<'_>,
    node: NodeId,
    name: &str,
    label: &str,
) -> Result<&'a str, AeroError> {
    doc.get_attribute(node, name)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| unauthorized(&format!("{label} is required")))
}

fn require_algorithm(
    doc: &Document<'_>,
    node: NodeId,
    expected: &str,
    label: &str,
) -> Result<(), AeroError> {
    let actual = required_attribute(doc, node, "Algorithm", "Algorithm")?;
    if actual == expected {
        Ok(())
    } else {
        Err(unauthorized(&format!("{label} algorithm is not allowed")))
    }
}

fn require_only_element_children(
    doc: &Document<'_>,
    parent: NodeId,
    expected: &[NodeId],
    label: &str,
) -> Result<(), AeroError> {
    let actual: Vec<_> = doc
        .children_iter(parent)
        .filter(|node| doc.element(*node).is_some())
        .collect();
    if actual == expected {
        Ok(())
    } else {
        Err(unauthorized(&format!(
            "{label} contains unexpected or reordered elements"
        )))
    }
}

fn require_no_element_children(
    doc: &Document<'_>,
    parent: NodeId,
    label: &str,
) -> Result<(), AeroError> {
    if doc
        .children_iter(parent)
        .all(|node| doc.element(node).is_none())
    {
        Ok(())
    } else {
        Err(unauthorized(&format!(
            "{label} contains unsupported transform parameters"
        )))
    }
}

fn matches_element(doc: &Document<'_>, node: NodeId, namespace: &str, name: &str) -> bool {
    doc.element(node)
        .is_some_and(|element| element.matches_name_ns(namespace, name))
}

fn unauthorized(message: &str) -> AeroError {
    AeroError::Unauthorized(format!("saml: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = r##"<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol"
        xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion">
      <saml:Assertion ID="assertion-1">
        <ds:Signature xmlns:ds="http://www.w3.org/2000/09/xmldsig#">
          <ds:SignedInfo>
            <ds:CanonicalizationMethod Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#"/>
            <ds:SignatureMethod Algorithm="http://www.w3.org/2001/04/xmldsig-more#rsa-sha256"/>
            <ds:Reference URI="#assertion-1">
              <ds:Transforms>
                <ds:Transform Algorithm="http://www.w3.org/2000/09/xmldsig#enveloped-signature"/>
                <ds:Transform Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#"/>
              </ds:Transforms>
              <ds:DigestMethod Algorithm="http://www.w3.org/2001/04/xmlenc#sha256"/>
              <ds:DigestValue>digest</ds:DigestValue>
            </ds:Reference>
          </ds:SignedInfo>
          <ds:SignatureValue>signature</ds:SignatureValue>
        </ds:Signature>
      </saml:Assertion>
    </samlp:Response>"##;

    #[test]
    fn accepts_only_the_narrow_enveloped_assertion_profile() {
        validate_signature_profile(PROFILE).expect("approved profile");
    }

    #[test]
    fn rejects_local_file_reference_before_crypto_verification() {
        let malicious = PROFILE.replace(r##"URI="#assertion-1""##, r#"URI="/dev/zero""#);
        let error = validate_signature_profile(&malicious).expect_err("local URI must fail");
        assert!(matches!(error, AeroError::Unauthorized(_)));
    }

    #[test]
    fn rejects_weak_algorithms_and_expensive_transforms() {
        let weak = PROFILE.replace(RSA_SHA256, "http://www.w3.org/2000/09/xmldsig#rsa-sha1");
        assert!(validate_signature_profile(&weak).is_err());

        let xpath = PROFILE.replace(
            r"<ds:Transforms>",
            r#"<ds:Transforms><ds:Transform Algorithm="http://www.w3.org/TR/1999/REC-xpath-19991116"><ds:XPath>//*</ds:XPath></ds:Transform>"#,
        );
        assert!(validate_signature_profile(&xpath).is_err());
    }

    #[test]
    fn rejects_multiple_references_before_any_digest_work() {
        let duplicate = PROFILE.replace(
            r"</ds:SignedInfo>",
            r##"<ds:Reference URI="#assertion-1"><ds:Transforms><ds:Transform Algorithm="http://www.w3.org/2000/09/xmldsig#enveloped-signature"/><ds:Transform Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#"/></ds:Transforms><ds:DigestMethod Algorithm="http://www.w3.org/2001/04/xmlenc#sha256"/><ds:DigestValue>digest</ds:DigestValue></ds:Reference></ds:SignedInfo>"##,
        );
        assert!(validate_signature_profile(&duplicate).is_err());
    }
}
