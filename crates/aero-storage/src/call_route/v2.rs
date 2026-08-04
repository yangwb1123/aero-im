use super::{
    markers_by_participant, merge_routes, normalize_node, CallId, CallRouteRegistry, RouteSnapshot,
};
use fred::prelude::{ClientLike, HashesInterface, KeysInterface, SetsInterface};
use fred::types::CustomCommand;
use std::collections::{BTreeSet, HashMap};

pub(super) const REGISTER_V2: &str = r"
redis.call('SET', KEYS[1], ARGV[1], 'EX', ARGV[3])
redis.call('DEL', KEYS[3])
redis.call('SADD', KEYS[2], ARGV[2])
redis.call('EXPIRE', KEYS[2], ARGV[4])
return 1
";

const REGISTER_V2_GENERATION: &str = r"
local function compare_i64(a, b)
  if a == b then return 0 end
  local a_negative = string.sub(a, 1, 1) == '-'
  local b_negative = string.sub(b, 1, 1) == '-'
  if a_negative ~= b_negative then return a_negative and -1 or 1 end
  local a_digits = a_negative and string.sub(a, 2) or a
  local b_digits = b_negative and string.sub(b, 2) or b
  local magnitude
  if string.len(a_digits) ~= string.len(b_digits) then
    magnitude = string.len(a_digits) > string.len(b_digits) and 1 or -1
  else
    magnitude = a_digits > b_digits and 1 or -1
  end
  return a_negative and -magnitude or magnitude
end
local current = redis.call('GET', KEYS[3])
if current and compare_i64(current, ARGV[5]) > 0 then
  return 0
end
redis.call('SET', KEYS[1], ARGV[1], 'EX', ARGV[3])
redis.call('SET', KEYS[3], ARGV[5], 'EX', ARGV[3])
redis.call('SADD', KEYS[2], ARGV[2])
redis.call('EXPIRE', KEYS[2], ARGV[4])
return 1
";

// Rotate the empty-valued compatibility marker on every heartbeat. The
// generation in the field name lets a concurrent stale-prune prove that no
// newer heartbeat changed the legacy mapping before deleting it.
pub(super) const REGISTER_LEGACY: &str = r"
redis.call('HSET', KEYS[1], ARGV[1], ARGV[2])
local fields = redis.call('HKEYS', KEYS[1])
for _, field in ipairs(fields) do
  if string.sub(field, 1, string.len(ARGV[3])) == ARGV[3] then
    redis.call('HDEL', KEYS[1], field)
  end
end
redis.call('HSET', KEYS[1], ARGV[4], '')
redis.call('EXPIRE', KEYS[1], ARGV[5])
return 1
";

// Generation-aware compatibility writes use an empty marker whose field name
// carries the generation. Old readers keep ignoring the empty value, while
// delayed older writers can no longer replace a newer node in the legacy hash.
const REGISTER_LEGACY_GENERATION: &str = r"
local function compare_i64(a, b)
  if a == b then return 0 end
  local a_negative = string.sub(a, 1, 1) == '-'
  local b_negative = string.sub(b, 1, 1) == '-'
  if a_negative ~= b_negative then return a_negative and -1 or 1 end
  local a_digits = a_negative and string.sub(a, 2) or a
  local b_digits = b_negative and string.sub(b, 2) or b
  local magnitude
  if string.len(a_digits) ~= string.len(b_digits) then
    magnitude = string.len(a_digits) > string.len(b_digits) and 1 or -1
  else
    magnitude = a_digits > b_digits and 1 or -1
  end
  return a_negative and -magnitude or magnitude
end
local newest = nil
local fields = redis.call('HKEYS', KEYS[1])
for _, field in ipairs(fields) do
  if string.sub(field, 1, string.len(ARGV[3])) == ARGV[3] then
    local suffix = string.sub(field, string.len(ARGV[3]) + 1)
    if string.sub(suffix, 1, 2) == 'g:' then
      local separator = string.find(suffix, ':', 3, true)
      if separator then
        local generation = string.sub(suffix, 3, separator - 1)
        if not newest or compare_i64(generation, newest) > 0 then
          newest = generation
        end
      end
    end
  end
end
if newest and compare_i64(newest, ARGV[5]) > 0 then
  return 0
end
redis.call('HSET', KEYS[1], ARGV[1], ARGV[2])
for _, field in ipairs(fields) do
  if string.sub(field, 1, string.len(ARGV[3])) == ARGV[3] then
    redis.call('HDEL', KEYS[1], field)
  end
end
redis.call('HSET', KEYS[1], ARGV[4], '')
redis.call('EXPIRE', KEYS[1], ARGV[6])
return 1
";

const PRUNE_V2_IF_EXPIRED: &str = r"
if redis.call('EXISTS', KEYS[1]) == 1 then
  return 0
end
redis.call('DEL', KEYS[3])
redis.call('SREM', KEYS[2], ARGV[1])
return 1
";

pub(super) const DELETE_V2: &str = r"
redis.call('DEL', KEYS[1])
redis.call('DEL', KEYS[3])
redis.call('SREM', KEYS[2], ARGV[1])
return 1
";

pub(super) const DELETE_V2_IF_NODE: &str = r"
local node = redis.call('GET', KEYS[1])
if node and node ~= ARGV[2] then
  return 0
end
redis.call('DEL', KEYS[1])
redis.call('DEL', KEYS[3])
redis.call('SREM', KEYS[2], ARGV[1])
if node then
  return 1
end
return -1
";

const DELETE_V2_GENERATION: &str = r"
local current_generation = redis.call('GET', KEYS[3])
if not current_generation or current_generation ~= ARGV[3] then
  return 0
end
local current_node = redis.call('GET', KEYS[1])
if not current_node or current_node ~= ARGV[2] then
  return 0
end
redis.call('DEL', KEYS[1])
redis.call('DEL', KEYS[3])
redis.call('SREM', KEYS[2], ARGV[1])
return 1
";

const DELETE_LEGACY_GENERATION: &str = r"
local function compare_i64(a, b)
  if a == b then return 0 end
  local a_negative = string.sub(a, 1, 1) == '-'
  local b_negative = string.sub(b, 1, 1) == '-'
  if a_negative ~= b_negative then return a_negative and -1 or 1 end
  local a_digits = a_negative and string.sub(a, 2) or a
  local b_digits = b_negative and string.sub(b, 2) or b
  local magnitude
  if string.len(a_digits) ~= string.len(b_digits) then
    magnitude = string.len(a_digits) > string.len(b_digits) and 1 or -1
  else
    magnitude = a_digits > b_digits and 1 or -1
  end
  return a_negative and -magnitude or magnitude
end
if redis.call('HGET', KEYS[1], ARGV[1]) ~= ARGV[2] then
  return 0
end
local exact = false
local newest = nil
local fields = redis.call('HKEYS', KEYS[1])
for _, field in ipairs(fields) do
  if string.sub(field, 1, string.len(ARGV[3])) == ARGV[3] then
    local suffix = string.sub(field, string.len(ARGV[3]) + 1)
    if string.sub(suffix, 1, 2) == 'g:' then
      local separator = string.find(suffix, ':', 3, true)
      if separator then
        local generation = string.sub(suffix, 3, separator - 1)
        if generation == ARGV[4] then
          exact = true
        end
        if not newest or compare_i64(generation, newest) > 0 then
          newest = generation
        end
      end
    end
  end
end
if not exact or newest ~= ARGV[4] then
  return 0
end
redis.call('HDEL', KEYS[1], ARGV[1])
for _, field in ipairs(fields) do
  if string.sub(field, 1, string.len(ARGV[3])) == ARGV[3] then
    redis.call('HDEL', KEYS[1], field)
  end
end
return 1
";

// Delete a legacy participant only when both its node and the exact marker
// generation set observed by the caller are unchanged. REGISTER_LEGACY rotates
// that set atomically, fencing a concurrent heartbeat.
const DELETE_LEGACY_IF_UNCHANGED: &str = r"
if ARGV[2] ~= '' and redis.call('HGET', KEYS[1], ARGV[1]) ~= ARGV[2] then
  return 0
end
local current = {}
local fields = redis.call('HKEYS', KEYS[1])
for _, field in ipairs(fields) do
  if string.sub(field, 1, string.len(ARGV[3])) == ARGV[3] then
    table.insert(current, field)
  end
end
if #current ~= tonumber(ARGV[4]) then
  return 0
end
for _, field in ipairs(current) do
  local found = false
  for i = 5, #ARGV do
    if field == ARGV[i] then
      found = true
      break
    end
  end
  if not found then
    return 0
  end
end
redis.call('HDEL', KEYS[1], ARGV[1])
for _, field in ipairs(current) do
  redis.call('HDEL', KEYS[1], field)
end
return 1
";

impl CallRouteRegistry {
    /// Register or refresh a route while fencing delayed work from an older
    /// media-session generation.
    ///
    /// The lease remains the plain node URL for compatibility; generation is
    /// stored in a colocated side key and compared atomically by Redis.
    pub async fn register_participant_generation(
        &self,
        call: CallId,
        participant: aero_common::ParticipantId,
        node_url: &str,
        generation: i64,
    ) -> anyhow::Result<bool> {
        let participant = participant.to_string();
        let node = normalize_node(node_url);
        let ttl = self.ttl_secs();
        let accepted = self
            .eval_i64(
                REGISTER_V2_GENERATION,
                vec![
                    Self::lease_key(call, &participant),
                    Self::index_key(call),
                    Self::generation_key(call, &participant),
                ],
                vec![
                    node.to_owned(),
                    participant.clone(),
                    ttl.to_string(),
                    ttl.saturating_mul(2).to_string(),
                    generation.to_string(),
                ],
            )
            .await?
            == 1;
        if !accepted {
            return Ok(false);
        }

        if self.v2_only {
            let _: () = self.client.del(Self::legacy_key(call)).await?;
            return Ok(true);
        }

        let marker_prefix = Self::marker_prefix(&participant);
        let marker = generation_marker(&marker_prefix, generation);
        self.eval_i64(
            REGISTER_LEGACY_GENERATION,
            vec![Self::legacy_key(call)],
            vec![
                participant,
                node.to_owned(),
                marker_prefix,
                marker,
                generation.to_string(),
                ttl.to_string(),
            ],
        )
        .await?;
        Ok(true)
    }

    /// Generation-aware alias of [`Self::register_participant_generation`].
    pub async fn heartbeat_generation(
        &self,
        call: CallId,
        participant: aero_common::ParticipantId,
        node_url: &str,
        generation: i64,
    ) -> anyhow::Result<bool> {
        self.register_participant_generation(call, participant, node_url, generation)
            .await
    }

    /// Remove a route only when both its node and generation still match.
    pub async fn unregister_participant_generation(
        &self,
        call: CallId,
        participant: aero_common::ParticipantId,
        node_url: &str,
        generation: i64,
    ) -> anyhow::Result<bool> {
        let participant = participant.to_string();
        let node = normalize_node(node_url);
        let deleted = self
            .eval_i64(
                DELETE_V2_GENERATION,
                vec![
                    Self::lease_key(call, &participant),
                    Self::index_key(call),
                    Self::generation_key(call, &participant),
                ],
                vec![participant.clone(), node.to_owned(), generation.to_string()],
            )
            .await?
            == 1;
        if !deleted {
            return Ok(false);
        }

        if self.v2_only {
            let _: () = self.client.del(Self::legacy_key(call)).await?;
            return Ok(true);
        }

        self.eval_i64(
            DELETE_LEGACY_GENERATION,
            vec![Self::legacy_key(call)],
            vec![
                participant.clone(),
                node.to_owned(),
                Self::marker_prefix(&participant),
                generation.to_string(),
            ],
        )
        .await?;
        Ok(true)
    }

    pub(super) async fn eval_i64(
        &self,
        script: &'static str,
        keys: Vec<String>,
        arguments: Vec<String>,
    ) -> anyhow::Result<i64> {
        let Some(hash_key) = keys.first() else {
            anyhow::bail!("call-route script requires at least one Redis key");
        };
        let command = CustomCommand::new_static("EVAL", hash_key.as_str(), false);
        let mut args = Vec::with_capacity(2 + keys.len() + arguments.len());
        args.push(script.to_owned());
        args.push(keys.len().to_string());
        args.extend(keys);
        args.extend(arguments);
        Ok(self.client.custom(command, args).await?)
    }

    pub(super) async fn snapshot(&self, call: CallId) -> anyhow::Result<RouteSnapshot> {
        let legacy_key = Self::legacy_key(call);
        let legacy: HashMap<String, String> = if self.v2_only {
            // R2 is intentionally destructive to the compatibility namespace:
            // after the fleet is upgraded, markerless old-node fields must not
            // be admitted or allowed to linger.
            let _: () = self.client.del(&legacy_key).await?;
            HashMap::new()
        } else {
            self.client.hgetall(&legacy_key).await?
        };
        let indexed: Vec<String> = self.client.smembers(Self::index_key(call)).await?;
        let legacy_markers = markers_by_participant(&legacy);
        let mut candidates: BTreeSet<String> = indexed.into_iter().collect();
        candidates.extend(legacy_markers.keys().cloned());

        let mut v2 = HashMap::new();
        for participant in &candidates {
            let lease_key = Self::lease_key(call, participant);
            let node: Option<String> = self.client.get(&lease_key).await?;
            if let Some(node) = node.filter(|node| !normalize_node(node).is_empty()) {
                v2.insert(participant.clone(), node);
                continue;
            }

            let pruned = self
                .eval_i64(
                    PRUNE_V2_IF_EXPIRED,
                    vec![
                        lease_key.clone(),
                        Self::index_key(call),
                        Self::generation_key(call, participant),
                    ],
                    vec![participant.clone()],
                )
                .await?;
            if pruned == 0 {
                // A heartbeat won the race after our GET. Read its lease rather
                // than transiently removing a live route from this census.
                if let Some(node) = self
                    .client
                    .get::<Option<String>, _>(&lease_key)
                    .await?
                    .filter(|node| !normalize_node(node).is_empty())
                {
                    v2.insert(participant.clone(), node);
                }
                continue;
            }

            let markers = legacy_markers
                .get(participant)
                .map_or(&[] as &[String], Vec::as_slice);
            if !markers.is_empty()
                && self
                    .client
                    .get::<Option<String>, _>(&lease_key)
                    .await?
                    .is_none()
            {
                self.delete_legacy_if_unchanged(&legacy_key, participant, "", markers)
                    .await?;
            }
        }

        Ok(RouteSnapshot {
            entries: merge_routes(&legacy, &v2, &candidates),
            legacy_markers,
        })
    }

    pub(super) async fn delete_legacy_if_unchanged(
        &self,
        legacy_key: &str,
        participant: &str,
        expected_node: &str,
        markers: &[String],
    ) -> anyhow::Result<()> {
        let marker_prefix = Self::marker_prefix(participant);
        let mut args = vec![
            participant.to_owned(),
            expected_node.to_owned(),
            marker_prefix,
            markers.len().to_string(),
        ];
        args.extend(markers.iter().cloned());
        self.eval_i64(
            DELETE_LEGACY_IF_UNCHANGED,
            vec![legacy_key.to_owned()],
            args,
        )
        .await?;
        Ok(())
    }
}

fn generation_marker(prefix: &str, generation: i64) -> String {
    format!("{prefix}g:{generation}:{}", ulid::Ulid::new())
}

#[cfg(test)]
mod generation_tests {
    use super::*;
    use aero_common::{CallId, ParticipantId};
    use fred::prelude::{ClientLike, HashesInterface, KeysInterface, RedisClient};

    #[test]
    fn generation_lua_fences_older_refresh_and_requires_exact_delete() {
        assert!(REGISTER_V2_GENERATION.contains("compare_i64(current, ARGV[5]) > 0"));
        assert!(REGISTER_V2_GENERATION.contains("redis.call('SET', KEYS[3], ARGV[5]"));
        assert!(DELETE_V2_GENERATION.contains("current_generation ~= ARGV[3]"));
        assert!(DELETE_V2_GENERATION.contains("current_node ~= ARGV[2]"));
    }

    #[test]
    fn generation_marker_stays_invisible_and_parses_as_its_participant() {
        let participant = ParticipantId::new().to_string();
        let prefix = CallRouteRegistry::marker_prefix(&participant);
        let marker = generation_marker(&prefix, 42);
        assert!(marker.starts_with(&format!("{prefix}g:42:")));
        assert_eq!(
            super::super::marker_participant(&marker),
            Some(participant.as_str())
        );
    }

    #[tokio::test]
    #[ignore = "requires live Redis"]
    async fn callroute_generation_rejects_late_heartbeat_and_leave() {
        let url =
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_owned());
        let client = RedisClient::new(
            fred::types::RedisConfig::from_url(&url).unwrap(),
            None,
            None,
            None,
        );
        client.connect();
        client.wait_for_connect().await.unwrap();

        let registry = CallRouteRegistry::new(client.clone()).with_v2_only(false);
        let call = CallId::new();
        let participant = ParticipantId::new();
        let old_generation = 9_007_199_254_740_992_i64;
        let current_generation = old_generation + 1;
        assert!(registry
            .register_participant_generation(
                call,
                participant,
                "http://node-a:3030",
                old_generation,
            )
            .await
            .unwrap());
        assert!(registry
            .register_participant_generation(
                call,
                participant,
                "http://node-b:3030",
                current_generation,
            )
            .await
            .unwrap());
        assert!(registry
            .heartbeat_generation(call, participant, "http://node-b:3030", current_generation,)
            .await
            .unwrap());
        assert!(!registry
            .heartbeat_generation(call, participant, "http://node-a:3030", old_generation,)
            .await
            .unwrap());
        assert!(!registry
            .unregister_participant_generation(
                call,
                participant,
                "http://node-a:3030",
                old_generation,
            )
            .await
            .unwrap());

        let lease: Option<String> = client
            .get(CallRouteRegistry::lease_key(call, &participant.to_string()))
            .await
            .unwrap();
        assert_eq!(lease.as_deref(), Some("http://node-b:3030"));
        let legacy_node: Option<String> = client
            .hget(CallRouteRegistry::legacy_key(call), participant.to_string())
            .await
            .unwrap();
        assert_eq!(legacy_node.as_deref(), Some("http://node-b:3030"));
        assert_eq!(
            registry.nodes_for_call(call).await.unwrap(),
            vec![("http://node-b:3030".to_owned(), 1)]
        );

        assert!(registry
            .unregister_participant_generation(
                call,
                participant,
                "http://node-b:3030",
                current_generation,
            )
            .await
            .unwrap());
        let _: i64 = client
            .del(CallRouteRegistry::index_key(call))
            .await
            .unwrap();
    }
}
