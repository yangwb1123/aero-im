// delivery_cursor.js — account-scoped, durable per-room delivery cursors.
//
// A cursor is written to localStorage before the WebSocket client acknowledges
// it to the server. That ordering makes a server-side delivery cursor mean the
// browser can recover the same Last-Known-Good point after a page reload. All
// keys include the authenticated participant id, so logging into another
// account in the same browser never imports the previous account's cursors.

export const DELIVERY_CURSOR_STORAGE_PREFIX = 'aero_delivery_cursors_v1:';
const STORAGE_VERSION = 2;
const MAX_PERSISTED_ROOMS = 512;

function browserStorage() {
  if (typeof window === 'undefined') return null;
  try { return window.localStorage || null; }
  catch { return null; }
}

function validId(value) {
  return typeof value === 'string' && value.length > 0 && value.length <= 128;
}

function validSeq(value) {
  // Zero is the explicit REST/backfill sentinel: those frames have a durable
  // message id but no NATS subject seq. Positive values are ordinary live
  // delivery floors.
  return Number.isSafeInteger(value) && value >= 0;
}

function validOrdinal(value) {
  return Number.isSafeInteger(value) && value > 0;
}

function storageKey(participantId) {
  return `${DELIVERY_CURSOR_STORAGE_PREFIX}${encodeURIComponent(participantId)}`;
}

function parseRooms(raw) {
  if (!raw) return new Map();
  try {
    const parsed = JSON.parse(raw);
    if (parsed?.version !== STORAGE_VERSION || !parsed.rooms
      || typeof parsed.rooms !== 'object' || Array.isArray(parsed.rooms)) {
      return new Map();
    }
    const rooms = new Map();
    for (const [roomId, cursor] of Object.entries(parsed.rooms)) {
      if (!validId(roomId) || !validId(cursor?.message_id)
        || !validOrdinal(cursor?.delivery_ordinal) || !validSeq(cursor?.seq)) continue;
      const updatedAt = Number(cursor.updated_at);
      rooms.set(roomId, {
        message_id: cursor.message_id,
        delivery_ordinal: cursor.delivery_ordinal,
        seq: cursor.seq,
        updated_at: Number.isFinite(updatedAt) ? updatedAt : 0,
      });
    }
    return rooms;
  } catch {
    return new Map();
  }
}

/** Durable, participant-scoped cursor ledger.
 *
 * `advance` returns the newly persisted cursor, or `null` for a stale value or
 * when durable storage is unavailable. Callers must only send a server ACK for
 * a non-null result (or an entry returned by `entries()`).
 */
export class DeliveryCursorLedger {
  constructor(storage = undefined) {
    this.storage = storage === undefined ? browserStorage() : storage;
    this.participantId = null;
    this.rooms = new Map();
  }

  open(participantId) {
    this.participantId = validId(participantId) ? participantId : null;
    this.rooms = new Map();
    if (!this.participantId || !this.storage) return;
    try {
      this.rooms = parseRooms(this.storage.getItem(storageKey(this.participantId)));
    } catch {
      this.rooms = new Map();
    }
  }

  entries() {
    return Array.from(this.rooms, ([room_id, cursor]) => ({
      room_id,
      message_id: cursor.message_id,
      delivery_ordinal: cursor.delivery_ordinal,
      seq: cursor.seq,
    }));
  }

  advance(roomId, messageId, deliveryOrdinal, seq) {
    if (!this.participantId || !this.storage
      || !validId(roomId) || !validId(messageId)
      || !validOrdinal(deliveryOrdinal) || !validSeq(seq)) return null;
    const previous = this.rooms.get(roomId);
    if (previous && deliveryOrdinal < previous.delivery_ordinal) return null;
    if (previous && deliveryOrdinal === previous.delivery_ordinal && seq <= previous.seq) return null;

    // The transactional room ordinal is the cumulative replay position.
    // Message ids are identifiers only: independently maximizing a ULID can skip
    // a lower id that is published later by a concurrent writer.
    const ordinalAdvanced = !previous || deliveryOrdinal > previous.delivery_ordinal;
    const next = {
      message_id: ordinalAdvanced ? messageId : previous.message_id,
      delivery_ordinal: ordinalAdvanced ? deliveryOrdinal : previous.delivery_ordinal,
      seq: previous ? Math.max(previous.seq, seq) : seq,
      updated_at: Date.now(),
    };
    this.rooms.set(roomId, next);
    this._prune();
    if (this._persist()) {
      return {
        room_id: roomId,
        message_id: next.message_id,
        delivery_ordinal: next.delivery_ordinal,
        seq: next.seq,
      };
    }

    // Fail closed: without durable browser storage this delivery must not move
    // the server LKG, otherwise a reload could skip a message the browser lost.
    if (previous) this.rooms.set(roomId, previous);
    else this.rooms.delete(roomId);
    return null;
  }

  _prune() {
    if (this.rooms.size <= MAX_PERSISTED_ROOMS) return;
    const oldest = Array.from(this.rooms.entries())
      .sort((a, b) => a[1].updated_at - b[1].updated_at)
      .slice(0, this.rooms.size - MAX_PERSISTED_ROOMS);
    for (const [roomId] of oldest) this.rooms.delete(roomId);
  }

  _persist() {
    if (!this.participantId || !this.storage) return false;
    const rooms = Object.fromEntries(this.rooms);
    try {
      this.storage.setItem(storageKey(this.participantId), JSON.stringify({
        version: STORAGE_VERSION,
        rooms,
      }));
      return true;
    } catch {
      return false;
    }
  }
}
