//! Hand-authored OpenAPI 3.0 document, served at `/openapi.json`. It is the contract the web module's
//! generated client consumes. A drift-guard test asserts every registered route appears here.
//!
//! (A full `utoipa`-generated spec would require annotating every handler and deriving `ToSchema` on the
//! many `serde_json::Value`-typed responses; this served document delivers the same web-unblocking
//! contract now. Request/response bodies are typed loosely as objects — enough for client generation.)

use serde_json::{json, Value};

fn op(summary: &str, auth: bool, tag: &str) -> Value {
    let mut o = json!({
        "summary": summary,
        "tags": [tag],
        "responses": { "200": { "description": "OK" } },
    });
    if auth {
        o["security"] = json!([{ "bearerAuth": [] }]);
        o["responses"]["401"] = json!({ "description": "unauthenticated" });
    }
    o
}

/// The full OpenAPI document.
pub fn openapi_doc() -> Value {
    json!({
        "openapi": "3.0.3",
        "info": {
            "title": "The Clock of Life — scoring service",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Life-expectancy estimate, why-breakdown, recommendations, evidence references, environment compare, accounts, admin + audit, GDPR, and aggregates."
        },
        "servers": [{ "url": "/" }],
        "components": {
            "securitySchemes": {
                "bearerAuth": { "type": "http", "scheme": "bearer", "bearerFormat": "JWT" }
            }
        },
        "paths": {
            "/health": { "get": op("Liveness", false, "system") },
            "/api/meta": { "get": op("Active model version + provenance", false, "model") },
            "/api/openapi.json": { "get": op("This OpenAPI document", false, "system") },

            "/api/auth/register": { "post": op("Create an account, return a bearer token", false, "auth") },
            "/api/auth/login": { "post": op("Verify credentials, return a bearer token", false, "auth") },

            "/api/questions": { "get": op("Interview definition (24 questions)", false, "interview") },
            "/api/references": { "get": op("Evidence studies; ?feature= or ?rule= filters", false, "evidence") },
            "/api/locations": { "get": op("Locations with PM2.5 / greenspace", false, "environment") },
            "/api/aggregates": { "get": op("k-anonymized cohort distributions", false, "aggregates") },

            "/api/estimate": { "post": op("Answers -> Life Clock + why[] + model; persists a calculation", false, "scoring") },
            "/api/recommendations": { "post": op("Prioritized, evidence-cited recommendations", false, "scoring") },
            "/api/whatif": { "post": op("Lifestyle-change overlay; persists a scenario if base given", false, "scoring") },
            "/api/relocate": { "post": op("Where Should I Live? location comparison", false, "environment") },

            "/api/calculations": { "get": op("The caller's calculation history", true, "history") },
            "/api/answers": {
                "get": op("Read the caller's answers", true, "interview"),
                "post": op("Upsert the caller's answers", true, "interview")
            },
            "/api/profile": { "get": op("The caller's profile + answers", true, "profile") },
            "/api/profile/location": { "post": op("Set the caller's home location", true, "profile") },
            "/api/account/export": { "get": op("GDPR export of all the caller's data", true, "privacy") },
            "/api/account": {
                "delete": op("GDPR erasure of the caller's account", true, "privacy"),
                "patch": op("Correct account data (locale)", true, "privacy")
            },

            "/api/admin/audit": { "get": op("Audit log (admin)", true, "admin") },
            "/api/admin/questions": { "post": op("Create a question (admin)", true, "admin") },
            "/api/admin/questions/{code}": { "put": op("Update a question (admin)", true, "admin") },
            "/api/admin/features/{key}": { "put": op("Update a feature (admin)", true, "admin") },
            "/api/admin/rules": { "post": op("Create a recommendation rule (admin)", true, "admin") },
            "/api/admin/rules/{code}": { "put": op("Update a recommendation rule (admin)", true, "admin") },
            "/api/admin/model/pin": { "post": op("Pin a model version active (admin)", true, "admin") }
        }
    })
}
