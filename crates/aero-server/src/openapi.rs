//! OpenAPI 3.0 spec endpoint — `/api/openapi.json`.
//!
//! Returns a static JSON document describing the public HTTP API surface.
//! The route is public (no auth extractor) so API clients and documentation
//! generators can reach it without credentials.

use axum::{routing::get, Json, Router};
use serde_json::Value;

use crate::state::AppState;

/// Router fragment for the `OpenAPI` spec endpoint.
///
/// Merges into the main router in `routes::build`.  No state is needed; the
/// spec is fully static.  Typed as `Router<AppState>` so it merges cleanly
/// into the stateful main router without a conversion error.
pub fn router() -> Router<AppState> {
    Router::new().route("/api/openapi.json", get(openapi_spec))
}

async fn openapi_spec() -> Json<Value> {
    Json(spec())
}

fn spec() -> Value {
    serde_json::json!({
        "openapi": "3.0.0",
        "info": {
            "title": "Aero IM API",
            "version": "1.0.0",
            "description": "Real-time messaging and live streaming platform"
        },
        "servers": [{ "url": "/api", "description": "API base" }],
        "paths": {
            "/messages/{id}": {
                "get": {
                    "summary": "Get a message",
                    "tags": ["messages"],
                    "parameters": [{
                        "name": "id",
                        "in": "path",
                        "required": true,
                        "schema": { "type": "string", "format": "uuid" }
                    }],
                    "responses": {
                        "200": { "description": "Message" },
                        "404": { "description": "Not found" }
                    }
                }
            },
            "/rooms/{id}/messages": {
                "get": {
                    "summary": "List room messages",
                    "tags": ["messages"],
                    "parameters": [{
                        "name": "id",
                        "in": "path",
                        "required": true,
                        "schema": { "type": "string" }
                    }],
                    "responses": {
                        "200": { "description": "Message list" }
                    }
                },
                "post": {
                    "summary": "Send a message",
                    "tags": ["messages"],
                    "parameters": [{
                        "name": "Idempotency-Key",
                        "in": "header",
                        "required": false,
                        "schema": { "type": "string", "format": "uuid" }
                    }],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": {
                                    "type": "object",
                                    "required": ["blocks"],
                                    "properties": {
                                        "blocks": { "type": "array", "maxItems": 50 },
                                        "reply_to": { "type": "string" },
                                        "expires_after_secs": { "type": "integer", "minimum": 0 },
                                        "client_message_id": { "type": "string", "format": "uuid" }
                                    }
                                }
                            }
                        }
                    },
                    "responses": {
                        "201": { "description": "Message created" },
                        "200": { "description": "Idempotent replay of the canonical message" },
                        "403": { "description": "Room access denied" },
                        "409": { "description": "Idempotency key reused with a different request" },
                        "429": { "description": "Workspace rate or slow-mode limit" }
                    }
                }
            },
            "/rooms": {
                "get": {
                    "summary": "List rooms",
                    "tags": ["rooms"],
                    "responses": {
                        "200": { "description": "Room list" }
                    }
                },
                "post": {
                    "summary": "Create a room",
                    "tags": ["rooms"],
                    "responses": {
                        "200": { "description": "Created room" }
                    }
                }
            },
            "/auth/register": {
                "post": {
                    "summary": "Register a new participant",
                    "tags": ["auth"],
                    "responses": {
                        "200": { "description": "Access and refresh tokens" }
                    }
                }
            },
            "/auth/login": {
                "post": {
                    "summary": "Login with password and optional TOTP or recovery code",
                    "tags": ["auth"],
                    "responses": {
                        "200": { "description": "Access and refresh tokens" },
                        "401": { "description": "Invalid credentials" }
                    }
                }
            },
            "/auth/2fa/recover": {
                "post": {
                    "summary": "Login with password and a one-time recovery code",
                    "tags": ["auth"],
                    "responses": {
                        "200": { "description": "Access and refresh tokens" },
                        "401": { "description": "Invalid credentials or recovery code" }
                    }
                }
            },
            "/me": {
                "get": {
                    "summary": "Get current participant",
                    "tags": ["participants"],
                    "responses": {
                        "200": { "description": "Participant" },
                        "401": { "description": "Unauthenticated" }
                    }
                }
            },
            "/streams": {
                "get": {
                    "summary": "List live streams",
                    "tags": ["streams"],
                    "responses": {
                        "200": { "description": "Stream list" }
                    }
                },
                "post": {
                    "summary": "Create a stream",
                    "tags": ["streams"],
                    "responses": {
                        "200": { "description": "Created stream with ingest URL" }
                    }
                }
            },
            "/streams/{id}": {
                "get": {
                    "summary": "Get a stream",
                    "tags": ["streams"],
                    "parameters": [{
                        "name": "id",
                        "in": "path",
                        "required": true,
                        "schema": { "type": "string" }
                    }],
                    "responses": {
                        "200": { "description": "Stream" },
                        "404": { "description": "Not found" }
                    }
                }
            }
        },
        "components": {
            "securitySchemes": {
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer",
                    "bearerFormat": "JWT"
                }
            }
        },
        "security": [{ "bearerAuth": [] }]
    })
}
