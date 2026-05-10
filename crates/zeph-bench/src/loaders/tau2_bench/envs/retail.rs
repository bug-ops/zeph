// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Retail domain environment for tau2-bench.
//!
//! Holds the in-memory state loaded from `db.json` and implements [`ToolExecutor`]
//! so the agent can call all 16 retail tools. Every call is recorded to the shared
//! [`ActionTrace`] before dispatch.

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
struct Address {
    address1: String,
    address2: String,
    city: String,
    state: String,
    zip: String,
    country: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
struct UserName {
    first_name: String,
    last_name: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
struct RetailUser {
    user_id: String,
    name: UserName,
    email: String,
    address: Address,
    payment_methods: serde_json::Map<String, serde_json::Value>,
    #[serde(default, flatten)]
    _rest: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
struct OrderItem {
    item_id: String,
    name: String,
    product_id: String,
    price: f64,
    #[serde(default)]
    options: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
struct RetailOrder {
    order_id: String,
    user_id: String,
    address: Address,
    items: Vec<OrderItem>,
    status: String,
    #[serde(default)]
    payment_history: Vec<serde_json::Value>,
    #[serde(default, flatten)]
    _rest: serde_json::Map<String, serde_json::Value>,
}

/// Full in-memory retail database.
///
/// Loaded once from `db.json` via [`RetailState::load`] and then cloned per scenario.
#[derive(Debug, Clone, Deserialize)]
struct RetailState {
    /// Product catalogue: `product_id → { name, variants: { item_id → { options, price, available } } }`.
    products: serde_json::Map<String, serde_json::Value>,
    /// User records by `user_id`.
    users: std::collections::HashMap<String, RetailUser>,
    /// Order records by `order_id`.
    orders: std::collections::HashMap<String, RetailOrder>,
}

impl RetailState {
    fn load(db_path: &Path) -> Result<Self, BenchError> {
        let file = std::fs::File::open(db_path)
            .map_err(|e| BenchError::InvalidFormat(format!("open retail db.json: {e}")))?;
        serde_json::from_reader(BufReader::new(file))
            .map_err(|e| BenchError::InvalidFormat(format!("parse retail db.json: {e}")))
    }
}

// ─── Executor ────────────────────────────────────────────────────────────────

/// In-memory retail environment executor for tau2-bench.
///
/// Holds mutable state (orders, users) behind a `std::sync::Mutex` and records every
/// tool call to the shared [`ActionTrace`] before dispatching.
///
/// # Construction
///
/// Always use [`RetailEnv::new_from_seed`] — it returns `(Self, ActionTrace)` where
/// the trace is the same `Arc` the env stores internally.
pub struct RetailEnv {
    state: Arc<Mutex<RetailState>>,
    trace: ActionTrace,
}

/// Load `RetailState` from `db_path`, memoising the result for the process lifetime.
///
/// The cache is keyed by the canonicalized (real) path so different relative paths to the
/// same file share an entry. Cache entries are never evicted — the process is short-lived
/// for benchmark runs, so unbounded growth is not a concern.
///
/// Lock poisoning falls through to a fresh disk reload; returning a valid state is always
/// preferable to propagating the poison error.
fn cached_retail_load(db_path: &Path) -> Result<RetailState, BenchError> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<std::path::PathBuf, Arc<RetailState>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    // Canonicalize so different relative-path spellings share the same entry.
    let key = std::fs::canonicalize(db_path).unwrap_or_else(|_| db_path.to_path_buf());

    // Fast path: cache hit — clone the Arc (cheap pointer bump) and return.
    if let Ok(guard) = cache.lock()
        && let Some(hit) = guard.get(&key)
    {
        return Ok((**hit).clone());
    }

    // Slow path: load from disk, then memoize.
    let state = RetailState::load(db_path)?;
    let arc = Arc::new(state.clone());
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, arc);
    }
    Ok(state)
}

impl RetailEnv {
    /// Load state from `db.json` and return `(env, trace)`.
    ///
    /// The returned `ActionTrace` is the same `Arc` stored inside the env. The evaluator
    /// must hold this clone to read recorded calls after the run completes.
    ///
    /// The `db.json` file is loaded once per process per unique path and memoised via
    /// a process-global cache. Each call receives an independent deep clone of the state
    /// so mutations in one scenario cannot affect another.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::InvalidFormat`] when `db.json` is missing or malformed.
    pub fn new_from_seed(db_path: &Path) -> Result<(Self, ActionTrace), BenchError> {
        let state = cached_retail_load(db_path)?;
        let trace: ActionTrace = Arc::new(Mutex::new(Vec::new()));
        let env = Self {
            state: Arc::new(Mutex::new(state)),
            trace: trace.clone(),
        };
        Ok((env, trace))
    }
}

impl ToolExecutor for RetailEnv {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        // tau2-bench uses structured tool calls only, not fenced code blocks.
        Ok(None)
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        super::tools::retail_definitions()
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        // Record before dispatching (lock held only for push, never across .await).
        {
            let mut t = self.trace.lock().expect("trace mutex poisoned");
            t.push(RecordedToolCall::from_tool_call(call));
        }

        let params = &call.params;
        let summary = match call.tool_id.as_str() {
            "calculate" => handle_calculate(params)?,
            "cancel_pending_order" => self.handle_cancel_pending_order(params)?,
            "exchange_delivered_order_items" => {
                self.handle_exchange_delivered_order_items(params)?
            }
            "find_user_id_by_email" => self.handle_find_user_id_by_email(params)?,
            "find_user_id_by_name_zip" => self.handle_find_user_id_by_name_zip(params)?,
            "get_order_details" => self.handle_get_order_details(params)?,
            "get_product_details" => self.handle_get_product_details(params)?,
            "get_item_details" => self.handle_get_item_details(params)?,
            "get_user_details" => self.handle_get_user_details(params)?,
            "list_all_product_types" => self.handle_list_all_product_types(),
            "modify_pending_order_address" => self.handle_modify_pending_order_address(params)?,
            "modify_pending_order_items" => self.handle_modify_pending_order_items(params)?,
            "modify_pending_order_payment" => self.handle_modify_pending_order_payment(params)?,
            "modify_user_address" => self.handle_modify_user_address(params)?,
            "return_delivered_order_items" => self.handle_return_delivered_order_items(params)?,
            "transfer_to_human_agents" => handle_transfer_to_human_agents(params)?,
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

// ─── Handlers ────────────────────────────────────────────────────────────────

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

fn params_str_list(
    params: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Vec<String>, ToolError> {
    params
        .get(key)
        .and_then(|v| v.as_array())
        .ok_or_else(|| ToolError::InvalidParams {
            message: format!("missing or non-array parameter '{key}'"),
        })
        .and_then(|arr| {
            arr.iter()
                .map(|v| {
                    v.as_str()
                        .map(ToOwned::to_owned)
                        .ok_or_else(|| ToolError::InvalidParams {
                            message: format!("array '{key}' contains non-string element"),
                        })
                })
                .collect()
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

impl RetailEnv {
    fn handle_cancel_pending_order(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let order_id = params_str(params, "order_id")?;
        let reason = params_str(params, "reason")?;
        let mut state = self.state.lock().expect("state mutex poisoned");
        let order = state
            .orders
            .get_mut(order_id)
            .ok_or_else(|| ToolError::InvalidParams {
                message: format!("order {order_id} not found"),
            })?;
        if order.status != "pending" {
            return Err(ToolError::InvalidParams {
                message: format!("order {order_id} is not pending (status={})", order.status),
            });
        }
        "cancelled".clone_into(&mut order.status);
        Ok(format!(
            "order_id={order_id} status=cancelled reason={reason}"
        ))
    }

    fn handle_exchange_delivered_order_items(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let order_id = params_str(params, "order_id")?;
        let item_ids = params_str_list(params, "item_ids")?;
        let new_item_ids = params_str_list(params, "new_item_ids")?;
        let payment_method_id = params_str(params, "payment_method_id")?;
        let mut state = self.state.lock().expect("state mutex poisoned");
        let order = state
            .orders
            .get_mut(order_id)
            .ok_or_else(|| ToolError::InvalidParams {
                message: format!("order {order_id} not found"),
            })?;
        if order.status != "delivered" {
            return Err(ToolError::InvalidParams {
                message: format!(
                    "order {order_id} is not delivered (status={})",
                    order.status
                ),
            });
        }
        order.items.retain(|item| !item_ids.contains(&item.item_id));
        for new_id in &new_item_ids {
            order.items.push(OrderItem {
                item_id: new_id.clone(),
                name: "exchanged_item".into(),
                product_id: String::new(),
                price: 0.0,
                options: serde_json::Map::new(),
            });
        }
        Ok(format!(
            "order_id={order_id} exchanged={item_ids:?} new_items={new_item_ids:?} payment_method_id={payment_method_id}"
        ))
    }

    fn handle_find_user_id_by_email(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let email = params_str(params, "email")?;
        let state = self.state.lock().expect("state mutex poisoned");
        let user = state
            .users
            .values()
            .find(|u| u.email.eq_ignore_ascii_case(email))
            .ok_or_else(|| ToolError::InvalidParams {
                message: format!("no user found with email {email}"),
            })?;
        Ok(format!("user_id={}", user.user_id))
    }

    fn handle_find_user_id_by_name_zip(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let first = params_str(params, "first_name")?;
        let last = params_str(params, "last_name")?;
        let zip = params_str(params, "zip")?;
        let state = self.state.lock().expect("state mutex poisoned");
        let user = state
            .users
            .values()
            .find(|u| {
                u.name.first_name.eq_ignore_ascii_case(first)
                    && u.name.last_name.eq_ignore_ascii_case(last)
                    && u.address.zip == zip
            })
            .ok_or_else(|| ToolError::InvalidParams {
                message: format!("no user found for {first} {last} zip={zip}"),
            })?;
        Ok(format!("user_id={}", user.user_id))
    }

    fn handle_get_order_details(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let order_id = params_str(params, "order_id")?;
        let state = self.state.lock().expect("state mutex poisoned");
        let order = state
            .orders
            .get(order_id)
            .ok_or_else(|| ToolError::InvalidParams {
                message: format!("order {order_id} not found"),
            })?;
        Ok(serde_json::to_string(order).unwrap_or_else(|_| format!("order_id={order_id}")))
    }

    fn handle_get_product_details(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let product_id = params_str(params, "product_id")?;
        let state = self.state.lock().expect("state mutex poisoned");
        let product = state
            .products
            .get(product_id)
            .ok_or_else(|| ToolError::InvalidParams {
                message: format!("product {product_id} not found"),
            })?;
        Ok(product.to_string())
    }

    fn handle_get_item_details(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let item_id = params_str(params, "item_id")?;
        let state = self.state.lock().expect("state mutex poisoned");
        // Items (variants) are nested inside each product's `variants` map.
        for product in state.products.values() {
            if let Some(variants) = product.get("variants").and_then(|v| v.as_object())
                && let Some(variant) = variants.get(item_id)
            {
                return Ok(variant.to_string());
            }
        }
        Err(ToolError::InvalidParams {
            message: format!("item {item_id} not found"),
        })
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

    fn handle_list_all_product_types(&self) -> String {
        let state = self.state.lock().expect("state mutex poisoned");
        let names: Vec<&str> = state
            .products
            .values()
            .filter_map(|p| p.get("name").and_then(|n| n.as_str()))
            .collect();
        format!("product_types={names:?}")
    }

    fn handle_modify_pending_order_address(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let order_id = params_str(params, "order_id")?;
        let address1 = params_str(params, "address1")?.to_owned();
        let address2 = params_str(params, "address2")?.to_owned();
        let city = params_str(params, "city")?.to_owned();
        let state_str = params_str(params, "state")?.to_owned();
        let zip = params_str(params, "zip")?.to_owned();
        let country = params_str(params, "country")?.to_owned();
        let mut state = self.state.lock().expect("state mutex poisoned");
        let order = state
            .orders
            .get_mut(order_id)
            .ok_or_else(|| ToolError::InvalidParams {
                message: format!("order {order_id} not found"),
            })?;
        if order.status != "pending" {
            return Err(ToolError::InvalidParams {
                message: format!("order {order_id} is not pending (status={})", order.status),
            });
        }
        order.address = Address {
            address1,
            address2,
            city,
            state: state_str,
            zip,
            country,
        };
        Ok(format!("order_id={order_id} address_updated=true"))
    }

    fn handle_modify_pending_order_items(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let order_id = params_str(params, "order_id")?;
        let item_ids = params_str_list(params, "item_ids")?;
        let new_item_ids = params_str_list(params, "new_item_ids")?;
        let payment_method_id = params_str(params, "payment_method_id")?;
        let mut state = self.state.lock().expect("state mutex poisoned");
        let order = state
            .orders
            .get_mut(order_id)
            .ok_or_else(|| ToolError::InvalidParams {
                message: format!("order {order_id} not found"),
            })?;
        if order.status != "pending" {
            return Err(ToolError::InvalidParams {
                message: format!("order {order_id} is not pending (status={})", order.status),
            });
        }
        order.items.retain(|item| !item_ids.contains(&item.item_id));
        for new_id in &new_item_ids {
            order.items.push(OrderItem {
                item_id: new_id.clone(),
                name: "new_item".into(),
                product_id: String::new(),
                price: 0.0,
                options: serde_json::Map::new(),
            });
        }
        Ok(format!(
            "order_id={order_id} removed={item_ids:?} added={new_item_ids:?} payment_method_id={payment_method_id}"
        ))
    }

    fn handle_modify_pending_order_payment(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let order_id = params_str(params, "order_id")?;
        let payment_method_id = params_str(params, "payment_method_id")?;
        let state = self.state.lock().expect("state mutex poisoned");
        let order = state
            .orders
            .get(order_id)
            .ok_or_else(|| ToolError::InvalidParams {
                message: format!("order {order_id} not found"),
            })?;
        if order.status != "pending" {
            return Err(ToolError::InvalidParams {
                message: format!("order {order_id} is not pending (status={})", order.status),
            });
        }
        Ok(format!(
            "order_id={order_id} payment_method_id={payment_method_id} updated=true"
        ))
    }

    fn handle_modify_user_address(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let user_id = params_str(params, "user_id")?;
        let address1 = params_str(params, "address1")?.to_owned();
        let address2 = params_str(params, "address2")?.to_owned();
        let city = params_str(params, "city")?.to_owned();
        let state_str = params_str(params, "state")?.to_owned();
        let zip = params_str(params, "zip")?.to_owned();
        let country = params_str(params, "country")?.to_owned();
        let mut state = self.state.lock().expect("state mutex poisoned");
        let user = state
            .users
            .get_mut(user_id)
            .ok_or_else(|| ToolError::InvalidParams {
                message: format!("user {user_id} not found"),
            })?;
        user.address = Address {
            address1,
            address2,
            city,
            state: state_str,
            zip,
            country,
        };
        Ok(format!("user_id={user_id} address_updated=true"))
    }

    fn handle_return_delivered_order_items(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, ToolError> {
        let order_id = params_str(params, "order_id")?;
        let item_ids = params_str_list(params, "item_ids")?;
        let payment_method_id = params_str(params, "payment_method_id")?;
        let mut state = self.state.lock().expect("state mutex poisoned");
        let order = state
            .orders
            .get_mut(order_id)
            .ok_or_else(|| ToolError::InvalidParams {
                message: format!("order {order_id} not found"),
            })?;
        if order.status != "delivered" {
            return Err(ToolError::InvalidParams {
                message: format!(
                    "order {order_id} is not delivered (status={})",
                    order.status
                ),
            });
        }
        let refund: f64 = order
            .items
            .iter()
            .filter(|i| item_ids.contains(&i.item_id))
            .map(|i| i.price)
            .sum();
        order.items.retain(|i| !item_ids.contains(&i.item_id));
        Ok(format!(
            "order_id={order_id} returned={item_ids:?} refund={refund:.2} payment_method_id={payment_method_id}"
        ))
    }
}

/// Minimal expression evaluator for `calculate` (handles +, -, *, / on f64).
fn eval_expr(expr: &str) -> String {
    // Simple left-to-right evaluation without precedence for MVP.
    // Handles: "1 + 2", "10 * 3.5", "100 / 4 - 5".
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

    const RETAIL_DB_MIN: &str = r##"{
        "products": {
            "prod_001": {
                "name": "T-Shirt",
                "product_id": "prod_001",
                "variants": {
                    "item_001": {
                        "item_id": "item_001",
                        "options": {"color": "blue", "size": "M"},
                        "available": true,
                        "price": 25.00
                    }
                }
            }
        },
        "users": {
            "alice_smith_1": {
                "user_id": "alice_smith_1",
                "name": {"first_name": "Alice", "last_name": "Smith"},
                "email": "alice@example.com",
                "address": {
                    "address1": "1 Main St",
                    "address2": "",
                    "city": "Boston",
                    "state": "MA",
                    "zip": "02101",
                    "country": "USA"
                },
                "payment_methods": {
                    "credit_card_1": {"source": "credit_card", "id": "credit_card_1"}
                }
            }
        },
        "orders": {
            "#W0001": {
                "order_id": "#W0001",
                "user_id": "alice_smith_1",
                "address": {
                    "address1": "1 Main St",
                    "address2": "",
                    "city": "Boston",
                    "state": "MA",
                    "zip": "02101",
                    "country": "USA"
                },
                "items": [
                    {
                        "item_id": "item_001",
                        "name": "T-Shirt",
                        "product_id": "prod_001",
                        "price": 25.00,
                        "options": {"color": "blue", "size": "M"}
                    }
                ],
                "status": "pending",
                "payment_history": [
                    {"transaction_type": "payment", "amount": 25.00, "payment_method_id": "credit_card_1"}
                ]
            },
            "#W0002": {
                "order_id": "#W0002",
                "user_id": "alice_smith_1",
                "address": {
                    "address1": "1 Main St",
                    "address2": "",
                    "city": "Boston",
                    "state": "MA",
                    "zip": "02101",
                    "country": "USA"
                },
                "items": [
                    {
                        "item_id": "item_001",
                        "name": "T-Shirt",
                        "product_id": "prod_001",
                        "price": 25.00,
                        "options": {"color": "blue", "size": "M"}
                    }
                ],
                "status": "delivered",
                "payment_history": []
            }
        }
    }"##;

    fn make_env() -> (RetailEnv, ActionTrace) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.json");
        std::fs::write(&db_path, RETAIL_DB_MIN).unwrap();
        // Keep the tempdir alive by leaking it — acceptable in tests.
        std::mem::forget(dir);
        RetailEnv::new_from_seed(&db_path).unwrap()
    }

    #[allow(clippy::needless_pass_by_value)]
    fn call(tool: &str, params: serde_json::Value) -> ToolCall {
        use zeph_common::ToolName;
        ToolCall {
            tool_id: ToolName::new(tool),
            params: params.as_object().cloned().unwrap_or_default(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
        }
    }

    #[tokio::test]
    async fn find_user_by_email() {
        let (env, _) = make_env();
        let c = call(
            "find_user_id_by_email",
            serde_json::json!({"email": "alice@example.com"}),
        );
        let out = env.execute_tool_call(&c).await.unwrap().unwrap();
        assert!(out.summary.contains("alice_smith_1"));
    }

    #[tokio::test]
    async fn find_user_by_name_zip() {
        let (env, _) = make_env();
        let c = call(
            "find_user_id_by_name_zip",
            serde_json::json!({"first_name": "Alice", "last_name": "Smith", "zip": "02101"}),
        );
        let out = env.execute_tool_call(&c).await.unwrap().unwrap();
        assert!(out.summary.contains("alice_smith_1"));
    }

    #[tokio::test]
    async fn cancel_pending_order_success() {
        let (env, trace) = make_env();
        let c = call(
            "cancel_pending_order",
            serde_json::json!({"order_id": "#W0001", "reason": "no_longer_needed"}),
        );
        let out = env.execute_tool_call(&c).await.unwrap().unwrap();
        assert!(out.summary.contains("cancelled"));
        assert_eq!(trace.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancel_non_pending_order_fails() {
        let (env, _) = make_env();
        let c = call(
            "cancel_pending_order",
            serde_json::json!({"order_id": "#W0002", "reason": "no_longer_needed"}),
        );
        let err = env.execute_tool_call(&c).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams { .. }));
    }

    #[tokio::test]
    async fn get_order_details_success() {
        let (env, _) = make_env();
        let c = call(
            "get_order_details",
            serde_json::json!({"order_id": "#W0001"}),
        );
        let out = env.execute_tool_call(&c).await.unwrap().unwrap();
        assert!(out.summary.contains("W0001") || out.summary.contains("pending"));
    }

    #[tokio::test]
    async fn trace_records_calls() {
        let (env, trace) = make_env();
        assert_eq!(Arc::strong_count(&trace), 2, "env must share the trace Arc");
        let c = call(
            "get_user_details",
            serde_json::json!({"user_id": "alice_smith_1"}),
        );
        let _ = env.execute_tool_call(&c).await;
        assert_eq!(trace.lock().unwrap().len(), 1);
        assert_eq!(trace.lock().unwrap()[0].name, "get_user_details");
    }

    #[test]
    fn eval_expr_add() {
        assert_eq!(eval_expr("1 + 2"), "3");
    }

    #[test]
    fn eval_expr_multiply() {
        assert_eq!(eval_expr("3 * 4"), "12");
    }

    #[test]
    fn eval_expr_divide_by_zero() {
        assert!(eval_expr("1 / 0").contains("zero"));
    }

    /// `new_from_seed` called twice with the same path must return independently mutable
    /// environments (mutations in one must not affect the other).
    #[tokio::test]
    async fn new_from_seed_returns_independent_copies() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db_iso.json");
        std::fs::write(&db_path, RETAIL_DB_MIN).unwrap();

        let (env1, _trace1) = RetailEnv::new_from_seed(&db_path).unwrap();
        let (env2, _trace2) = RetailEnv::new_from_seed(&db_path).unwrap();

        // Cancel the pending order #W0001 in env1.
        let c = call(
            "cancel_pending_order",
            serde_json::json!({"order_id": "#W0001", "reason": "changed mind"}),
        );
        env1.execute_tool_call(&c).await.unwrap();

        // env2 must still have the order.
        let get = call(
            "get_order_details",
            serde_json::json!({"order_id": "#W0001"}),
        );
        assert!(
            env2.execute_tool_call(&get).await.unwrap().is_some(),
            "mutation in env1 must not affect env2"
        );
    }
}
