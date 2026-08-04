// Convert Aero's snake_case API payload (and the shared camelCase Rust shape)
// into the RTCConfiguration dictionary consumed by RTCPeerConnection.

const DEFAULT_ICE_SERVERS = [{ urls: 'stun:stun.l.google.com:19302' }];

export function browserRtcConfig(value) {
  if (!value || typeof value !== 'object') {
    return { iceServers: DEFAULT_ICE_SERVERS };
  }

  const servers = value.ice_servers || value.iceServers;
  const config = {
    iceServers: Array.isArray(servers) ? servers : [],
  };
  const policy = value.ice_transport_policy || value.iceTransportPolicy;
  if (policy === 'all' || policy === 'relay') config.iceTransportPolicy = policy;
  return config;
}
