// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Airline domain environment for tau2-bench.
//!
//! Holds the in-memory state loaded from `db.json` and implements [`ToolExecutor`]
//! so the agent can call all 14 airline tools.

use std::io::BufReader;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use zeph_tools::ToolExecutor;
use zeph_tools::executor::{ToolCall, ToolError, ToolOutput};
use zeph_tools::registry::ToolDef;

use crate::error::BenchError;

use super::{ActionTrace, RecordedToolCall};

// ─── State types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
struct AirlineUser {
    user_id: String,
    #[serde(default)]
    name: serde_json::Value,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    payment_methods: serde_json::Map<String, serde_json::Value>,
    #[serde(default, flatten)]
    _rest: serde_json::Map<String, serde_json::Value>,
}

#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
struct Reservation {
    reservation_id: String,
    user_id: String,
    origin: String,
    destination: String,
    #[serde(default)]
    flight_type: String,
    #[serde(default)]
    cabin: String,
    #[serde(default)]
    flights: Vec<serde_json::Value>,
    #[serde(default)]
    passengers: Vec<serde_json::Value>,
    #[serde(default)]
    payment_history: Vec<serde_json::Value>,
    #[serde(default)]
    total_baggages: u32,
    #[serde(default)]
    nonfree_baggages: u32,
    #[serde(default)]
    insurance: String,
    #[serde(default, flatten)]
    _rest: serde_json::Map<String, serde_json::Value>,
}

/// Full in-memory airline database.
#[derive(Debug, Clone, Deserialize)]
struct AirlineState {
    /// Flight data: `flight_number → { ... }`.
    flights: serde_json::Map<String, serde_json::Value>,
    /// User records by `user_id`.
    users: std::collections::HashMap<String, AirlineUser>,
    /// Reservation records by `reservation_id`.
    reservations: std::collections::HashMap<String, Reservation>,
}

impl AirlineState {
    fn load(db_path: &Path) -> Result<Self, BenchError> {
        let file = std::fs::File::open(db_path)
            .map_err(|e| BenchError::InvalidFormat(format!("open airline db.json: {e}")))?;
        serde_json::from_reader(BufReader::new(file))
            .map_err(|e| BenchError::InvalidFormat(format!("parse airline db.json: {e}")))
    }
}

// ─── Executor ────────────────────────────────────────────────────────────────

/// In-memory airline environment executor for tau2-bench.
///
/// # Construction
///
/// Always use [`AirlineEnv::new_from_seed`] — it returns `(Self, ActionTrace)` where
/// the trace is the same `Arc` the env stores internally.
pub struct AirlineEnv {
    state: Arc<Mutex<AirlineState>>,
    trace: ActionTrace,
}

impl AirlineEnv {
    /// Load state from `db.json` and return `(env, trace)`.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::InvalidFormat`] when `db.json` is missing or malformed.
    pub fn new_from_seed(db_path: &Path) -> Result<(Self, ActionTrace), BenchError> {
        // TODO(#3417/D3): cache the loaded state to avoid re-reading db.json per scenario.
        let state = AirlineState::load(db_path)?;
        let trace: ActionTrace = Arc::new(Mutex::new(Vec::new()));
        let env = Self {
            state: Arc::new(Mutex::new(state)),
            trace: trace.clone(),
        };
        Ok((env, trace))
    }
}

impl ToolExecutor for AirlineEnv {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        super::tools::airline_definitions()
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        {
            let mut t = self.trace.lock().expect("trace mutex poisoned");
            t.push(RecordedToolCall::from_tool_call(call));
        }

        let params = &call.params;
        let summary = match call.tool_id.as_str() {
            "book_reservation" => self.handle_book_reservation(params)?,
            "calculate" => handle_calculate(params)?,
            "cancel_reservation" => self.handle_cancel_reservation(params)?,
            "get_reservation_details" => self.handle_get_reservation_details(params)?,
            "get_user_details" => self.handle_get_user_details(params)?,
            "list_all_airports" => self.handle_list_all_airports(),
            "search_direct_flight" => self.handle_search_direct_flight(params)?,
            "search_onestop_flight" => self.handle_search_onestop_flight(params)?,
            "send_certificate" => self.handle_send_certificate(params)?,
            "transfer_to_human_agents" => handle_transfer_to_human_agents(params)?,
            "update_reservation_baggages" => self.handle_update_reservation_baggages(params)?,
            "update_reservation_flights" => self.handle_update_reservation_flights(params)?,
            "update_reservation_passengers" => self.handle_update_reservation_passengers(params)?,
            "get_flight_status" => self.handle_get_flight_status(params)?,
            _ => return Ok(None),
        };

        Ok(Some(ToolOutput {
            tool_name: call.tool_id.clone(),
            summary,
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: None,
        }))
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn params_str<'a>(
    params: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<&'a str, ToolError> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidParams {
            message: format!("missing or non-string parameter '{key}'"),
        })
}

fn handle_calculate(
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, ToolError> {
    let expr = params_str(params, "expression")?;
    Ok(format!("expression={expr} result={}", eval_expr(expr)))
}

fn handle_transfer_to_human_agents(
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, ToolError> {
    let summary = params_str(params, "summary")?;
    Ok(format!("transferred_to_human=true summary={summary:?}"))
}

// ─── Handlers ────────────────────────────────────────────────────────────────

impl AirlineEnv {
    fn handle_book_reservation(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let user_id = params_str(params, "user_id")?;
        let origin = params_str(params, "origin")?;
        let destination = params_str(params, "destination")?;
        let flight_type = params_str(params, "flight_type").unwrap_or("one_way");
        let cabin = params_str(params, "cabin").unwrap_or("economy");
        let flights = params
            .get("flights")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let passengers = params
            .get("passengers")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let payment_method_id = params_str(params, "payment_method_id").unwrap_or("");
        let total_baggages = u32::try_from(
            params
                .get("total_baggages")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        )
        .unwrap_or(u32::MAX);
        let nonfree_baggages = u32::try_from(
            params
                .get("nonfree_baggages")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        )
        .unwrap_or(u32::MAX);
        let insurance = params_str(params, "insurance").unwrap_or("no");

        // Generate a simple reservation id.
        let reservation_id = format!(
            "RES{:06}",
            self.state.lock().expect("poisoned").reservations.len() + 1
        );
        let res = Reservation {
            reservation_id: reservation_id.clone(),
            user_id: user_id.to_owned(),
            origin: origin.to_owned(),
            destination: destination.to_owned(),
            flight_type: flight_type.to_owned(),
            cabin: cabin.to_owned(),
            flights,
            passengers,
            payment_history: vec![
                serde_json::json!({"payment_id": payment_method_id, "amount": 0}),
            ],
            total_baggages,
            nonfree_baggages,
            insurance: insurance.to_owned(),
            _rest: serde_json::Map::new(),
        };
        self.state
            .lock()
            .expect("state mutex poisoned")
            .reservations
            .insert(reservation_id.clone(), res);
        Ok(format!(
            "reservation_id={reservation_id} user_id={user_id} origin={origin} destination={destination}"
        ))
    }

    fn handle_cancel_reservation(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let reservation_id = params_str(params, "reservation_id")?;
        let mut state = self.state.lock().expect("state mutex poisoned");
        state
            .reservations
            .remove(reservation_id)
            .ok_or_else(|| ToolError::InvalidParams {
                message: format!("reservation {reservation_id} not found"),
            })?;
        Ok(format!(
            "reservation_id={reservation_id} cancelled=true refund_issued=true"
        ))
    }

    fn handle_get_reservation_details(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let reservation_id = params_str(params, "reservation_id")?;
        let state = self.state.lock().expect("state mutex poisoned");
        let res =
            state
                .reservations
                .get(reservation_id)
                .ok_or_else(|| ToolError::InvalidParams {
                    message: format!("reservation {reservation_id} not found"),
                })?;
        Ok(serde_json::to_string(res)
            .unwrap_or_else(|_| format!("reservation_id={reservation_id}")))
    }

    fn handle_get_user_details(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let user_id = params_str(params, "user_id")?;
        let state = self.state.lock().expect("state mutex poisoned");
        let user = state
            .users
            .get(user_id)
            .ok_or_else(|| ToolError::InvalidParams {
                message: format!("user {user_id} not found"),
            })?;
        Ok(serde_json::to_string(user).unwrap_or_else(|_| format!("user_id={user_id}")))
    }

    fn handle_list_all_airports(&self) -> String {
        // Collect airport codes from existing flights in the DB.
        let state = self.state.lock().expect("state mutex poisoned");
        let mut airports: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for res in state.reservations.values() {
            airports.insert(res.origin.clone());
            airports.insert(res.destination.clone());
        }
        format!("airports={:?}", airports.into_iter().collect::<Vec<_>>())
    }

    fn handle_search_direct_flight(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let origin = params_str(params, "origin")?;
        let destination = params_str(params, "destination")?;
        let date = params_str(params, "date")?;
        let state = self.state.lock().expect("state mutex poisoned");
        let results: Vec<&serde_json::Value> = state
            .flights
            .values()
            .filter(|f| {
                f.get("origin").and_then(|v| v.as_str()) == Some(origin)
                    && f.get("destination").and_then(|v| v.as_str()) == Some(destination)
                    && f.get("dates")
                        .and_then(|v| v.as_array())
                        .is_some_and(|dates| dates.iter().any(|d| d.as_str() == Some(date)))
            })
            .collect();
        Ok(format!(
            "origin={origin} destination={destination} date={date} flights_found={}",
            results.len()
        ))
    }

    fn handle_search_onestop_flight(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let origin = params_str(params, "origin")?;
        let destination = params_str(params, "destination")?;
        let date = params_str(params, "date")?;
        // For MVP: return a simple count of flights that match origin or destination on that date.
        let state = self.state.lock().expect("state mutex poisoned");
        let relevant = state
            .flights
            .values()
            .filter(|f| {
                f.get("dates")
                    .and_then(|v| v.as_array())
                    .is_some_and(|dates| dates.iter().any(|d| d.as_str() == Some(date)))
            })
            .count();
        Ok(format!(
            "origin={origin} destination={destination} date={date} onestop_options={relevant}"
        ))
    }

    fn handle_send_certificate(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let user_id = params_str(params, "user_id")?;
        let amount = params
            .get("amount")
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(|| ToolError::InvalidParams {
                message: "missing or non-numeric parameter 'amount'".into(),
            })?;
        let state = self.state.lock().expect("state mutex poisoned");
        if !state.users.contains_key(user_id) {
            return Err(ToolError::InvalidParams {
                message: format!("user {user_id} not found"),
            });
        }
        Ok(format!(
            "user_id={user_id} certificate_amount={amount:.2} sent=true"
        ))
    }

    fn handle_update_reservation_baggages(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let reservation_id = params_str(params, "reservation_id")?;
        let total = u32::try_from(
            params
                .get("total_baggages")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| ToolError::InvalidParams {
                    message: "missing 'total_baggages'".into(),
                })?,
        )
        .unwrap_or(u32::MAX);
        let nonfree = u32::try_from(
            params
                .get("nonfree_baggages")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| ToolError::InvalidParams {
                    message: "missing 'nonfree_baggages'".into(),
                })?,
        )
        .unwrap_or(u32::MAX);
        let payment_method_id = params_str(params, "payment_method_id")?;
        let mut state = self.state.lock().expect("state mutex poisoned");
        let res =
            state
                .reservations
                .get_mut(reservation_id)
                .ok_or_else(|| ToolError::InvalidParams {
                    message: format!("reservation {reservation_id} not found"),
                })?;
        res.total_baggages = total;
        res.nonfree_baggages = nonfree;
        Ok(format!(
            "reservation_id={reservation_id} total_baggages={total} nonfree_baggages={nonfree} payment_method_id={payment_method_id}"
        ))
    }

    fn handle_update_reservation_flights(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let reservation_id = params_str(params, "reservation_id")?;
        let cabin = params_str(params, "cabin")?;
        let flights = params
            .get("flights")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let payment_method_id = params_str(params, "payment_method_id")?;
        let mut state = self.state.lock().expect("state mutex poisoned");
        let res =
            state
                .reservations
                .get_mut(reservation_id)
                .ok_or_else(|| ToolError::InvalidParams {
                    message: format!("reservation {reservation_id} not found"),
                })?;
        cabin.clone_into(&mut res.cabin);
        res.flights = flights;
        Ok(format!(
            "reservation_id={reservation_id} cabin={cabin} flights_updated=true payment_method_id={payment_method_id}"
        ))
    }

    fn handle_update_reservation_passengers(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let reservation_id = params_str(params, "reservation_id")?;
        let passengers = params
            .get("passengers")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut state = self.state.lock().expect("state mutex poisoned");
        let res =
            state
                .reservations
                .get_mut(reservation_id)
                .ok_or_else(|| ToolError::InvalidParams {
                    message: format!("reservation {reservation_id} not found"),
                })?;
        res.passengers = passengers;
        Ok(format!(
            "reservation_id={reservation_id} passengers_updated=true"
        ))
    }

    fn handle_get_flight_status(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let flight_number = params_str(params, "flight_number")?;
        let date = params_str(params, "date")?;
        let state = self.state.lock().expect("state mutex poisoned");
        if let Some(flight) = state.flights.get(flight_number) {
            return Ok(format!(
                "flight_number={flight_number} date={date} info={flight}"
            ));
        }
        Ok(format!(
            "flight_number={flight_number} date={date} status=unknown"
        ))
    }
}

fn eval_expr(expr: &str) -> String {
    let tokens: Vec<&str> = expr.split_whitespace().collect();
    if tokens.is_empty() {
        return "NaN".into();
    }
    let mut result: f64 = match tokens[0].parse() {
        Ok(v) => v,
        Err(_) => return "NaN".into(),
    };
    let mut i = 1;
    while i + 1 < tokens.len() {
        let op = tokens[i];
        let right: f64 = match tokens[i + 1].parse() {
            Ok(v) => v,
            Err(_) => return "NaN".into(),
        };
        match op {
            "+" => result += right,
            "-" => result -= right,
            "*" => result *= right,
            "/" => {
                if right == 0.0 {
                    return "division by zero".into();
                }
                result /= right;
            }
            _ => return format!("unknown op: {op}"),
        }
        i += 2;
    }
    format!("{result}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const AIRLINE_DB_MIN: &str = r#"{
        "flights": {
            "HAT001": {
                "flight_number": "HAT001",
                "origin": "JFK",
                "destination": "LAX",
                "dates": ["2024-06-01"],
                "price": 500
            }
        },
        "users": {
            "user_001": {
                "user_id": "user_001",
                "name": {"first_name": "Test", "last_name": "User"},
                "email": "test@example.com",
                "payment_methods": {
                    "credit_card_1": {"source": "credit_card", "id": "credit_card_1"}
                }
            }
        },
        "reservations": {
            "RES001": {
                "reservation_id": "RES001",
                "user_id": "user_001",
                "origin": "JFK",
                "destination": "LAX",
                "flight_type": "one_way",
                "cabin": "economy",
                "flights": [{"flight_number": "HAT001", "date": "2024-06-01", "price": 500}],
                "passengers": [{"first_name": "Test", "last_name": "User", "dob": "1990-01-01"}],
                "payment_history": [{"payment_id": "credit_card_1", "amount": 500}],
                "total_baggages": 1,
                "nonfree_baggages": 0,
                "insurance": "no"
            }
        }
    }"#;

    fn make_env() -> (AirlineEnv, ActionTrace) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.json");
        std::fs::write(&db_path, AIRLINE_DB_MIN).unwrap();
        std::mem::forget(dir);
        AirlineEnv::new_from_seed(&db_path).unwrap()
    }

    #[allow(clippy::needless_pass_by_value)]
    fn call(tool: &str, params: serde_json::Value) -> ToolCall {
        use zeph_common::ToolName;
        ToolCall {
            tool_id: ToolName::new(tool),
            params: params.as_object().cloned().unwrap_or_default(),
            caller_id: None,
        }
    }

    #[tokio::test]
    async fn get_reservation_details() {
        let (env, _) = make_env();
        let c = call(
            "get_reservation_details",
            serde_json::json!({"reservation_id": "RES001"}),
        );
        let out = env.execute_tool_call(&c).await.unwrap().unwrap();
        assert!(out.summary.contains("RES001"));
    }

    #[tokio::test]
    async fn cancel_reservation_success() {
        let (env, trace) = make_env();
        let c = call(
            "cancel_reservation",
            serde_json::json!({"reservation_id": "RES001"}),
        );
        let out = env.execute_tool_call(&c).await.unwrap().unwrap();
        assert!(out.summary.contains("cancelled=true"));
        assert_eq!(trace.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancel_missing_reservation_fails() {
        let (env, _) = make_env();
        let c = call(
            "cancel_reservation",
            serde_json::json!({"reservation_id": "DOESNOTEXIST"}),
        );
        assert!(env.execute_tool_call(&c).await.is_err());
    }

    #[tokio::test]
    async fn trace_records_all_calls() {
        let (env, trace) = make_env();
        assert!(Arc::strong_count(&trace) >= 2);
        let c1 = call(
            "get_user_details",
            serde_json::json!({"user_id": "user_001"}),
        );
        let c2 = call(
            "get_reservation_details",
            serde_json::json!({"reservation_id": "RES001"}),
        );
        let _ = env.execute_tool_call(&c1).await;
        let _ = env.execute_tool_call(&c2).await;
        assert_eq!(trace.lock().unwrap().len(), 2);
    }
}
