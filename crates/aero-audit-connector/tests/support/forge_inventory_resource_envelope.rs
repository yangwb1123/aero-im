use serde_json::Value;

pub(super) fn validate_envelope(value: &Value) -> Result<(), String> {
    let object = super::exact_object(
        value,
        &[
            "schema_version",
            "evaluation_mode",
            "inventory",
            "resource_view",
            "converged",
            "read_only",
            "authority",
        ],
    )?;
    if object.get("schema_version").and_then(Value::as_str)
        != Some("forge.device-inventory-resource-convergence/v1")
        || object.get("evaluation_mode").and_then(Value::as_str)
            != Some("owner_bound_inventory_resource_convergence_only")
    {
        return Err("convergence schema drifted".to_owned());
    }
    super::bool_field(object, "converged", true)?;
    super::bool_field(object, "read_only", true)?;
    super::authority(
        object
            .get("authority")
            .ok_or_else(|| "convergence authority missing".to_owned())?,
        &[
            "inventory_authoritative",
            "device_identity_verified",
            "reservation_created",
            "lease_issued",
            "execution_authorized",
            "dispatch_performed",
            "audit_published",
        ],
    )?;
    let (owner, inventory_devices) = super::validate_inventory(
        object
            .get("inventory")
            .ok_or_else(|| "inventory missing".to_owned())?,
    )?;
    let resource_devices = super::validate_resource(
        object
            .get("resource_view")
            .ok_or_else(|| "resource view missing".to_owned())?,
        owner,
    )?;
    let inventory_rows = inventory_devices
        .as_array()
        .ok_or_else(|| "inventory devices must be an array".to_owned())?;
    let resource_rows = resource_devices
        .as_array()
        .ok_or_else(|| "resource devices must be an array".to_owned())?;
    if inventory_rows.len() != resource_rows.len() {
        return Err("inventory/resource row count drifted".to_owned());
    }
    for row in inventory_rows {
        let candidate = row.as_object().unwrap();
        let instance = super::string_field(candidate, "instance_id")?;
        let device = candidate.get("device").unwrap().as_object().unwrap();
        let device_id = super::string_field(device, "device_id")?;
        let matching = resource_rows.iter().find(|value: &&Value| {
            value.get("device_id").and_then(Value::as_str) == Some(device_id)
        });
        let matching = matching.ok_or_else(|| "resource device identity missing".to_owned())?;
        if matching.get("runner_instance_id").and_then(Value::as_str) != Some(instance)
            || matching.get("revision") != candidate.get("revision")
            || matching.get("generation") != candidate.get("generation")
            || matching.get("heartbeat_sequence") != candidate.get("heartbeat_sequence")
            || matching.get("observed_at_ms") != device.get("snapshot_observed_at_ms")
        {
            return Err("inventory/resource lifecycle identity drifted".to_owned());
        }
    }
    Ok(())
}
