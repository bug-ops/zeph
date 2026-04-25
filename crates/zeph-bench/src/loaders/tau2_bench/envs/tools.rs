// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool definitions for tau2-bench retail and airline domains.
//!
//! Each domain exposes a set of structured tools the agent can invoke.
//! Definitions follow the schemars 1.x pattern used by `zeph-tools`.

// Param structs exist solely for schemars schema derivation; their fields are
// intentionally not read by Rust code — they are read by the LLM via JSON schema.
#![allow(dead_code)]

use schemars::JsonSchema;
use serde::Deserialize;
use zeph_tools::registry::{InvocationHint, ToolDef};

// ─── Retail shared params ────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct CalculateParams {
    /// Mathematical expression to evaluate (e.g. `"1 + 2 * 3"`).
    pub expression: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct CancelPendingOrderParams {
    /// Order id (e.g. `"#W1234567"`).
    pub order_id: String,
    /// Reason for cancellation: one of `no_longer_needed`, `ordered_by_mistake`.
    pub reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct ExchangeDeliveredOrderItemsParams {
    /// Order id (e.g. `"#W1234567"`).
    pub order_id: String,
    /// List of item ids to exchange.
    pub item_ids: Vec<String>,
    /// New item ids to exchange into.
    pub new_item_ids: Vec<String>,
    /// Payment method id to charge any delta.
    pub payment_method_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct FindUserIdByEmailParams {
    /// User's email address.
    pub email: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct FindUserIdByNameZipParams {
    /// User's first name.
    pub first_name: String,
    /// User's last name.
    pub last_name: String,
    /// User's ZIP code.
    pub zip: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct GetOrderDetailsParams {
    /// Order id (e.g. `"#W1234567"`).
    pub order_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct GetProductDetailsParams {
    /// Product id.
    pub product_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct GetItemDetailsParams {
    /// Item id (variant id).
    pub item_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct GetUserDetailsParams {
    /// User id.
    pub user_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct ModifyPendingOrderAddressParams {
    /// Order id.
    pub order_id: String,
    /// New address line 1.
    pub address1: String,
    /// New address line 2.
    pub address2: String,
    /// City.
    pub city: String,
    /// State.
    pub state: String,
    /// ZIP code.
    pub zip: String,
    /// Country.
    pub country: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct ModifyPendingOrderItemsParams {
    /// Order id.
    pub order_id: String,
    /// Item ids to remove.
    pub item_ids: Vec<String>,
    /// New item ids to add.
    pub new_item_ids: Vec<String>,
    /// Payment method id to charge any delta.
    pub payment_method_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct ModifyPendingOrderPaymentParams {
    /// Order id.
    pub order_id: String,
    /// New payment method id.
    pub payment_method_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct ModifyUserAddressParams {
    /// User id.
    pub user_id: String,
    /// New address line 1.
    pub address1: String,
    /// New address line 2.
    pub address2: String,
    /// City.
    pub city: String,
    /// State.
    pub state: String,
    /// ZIP code.
    pub zip: String,
    /// Country.
    pub country: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct ReturnDeliveredOrderItemsParams {
    /// Order id.
    pub order_id: String,
    /// List of item ids to return.
    pub item_ids: Vec<String>,
    /// Payment method id for refund.
    pub payment_method_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct TransferToHumanAgentsParams {
    /// Reason for transfer.
    pub summary: String,
}

/// Empty-object schema for tools that take no parameters.
///
/// LLM providers require `type: "object"` even for no-arg tools; `schemars::schema_for!(())`
/// produces `type: "null"` which most providers reject.
fn empty_object_schema() -> schemars::Schema {
    serde_json::from_value(serde_json::json!({"type": "object", "properties": {}}))
        .expect("static schema is valid")
}

/// Return all tool definitions for the retail domain.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn retail_definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            id: "calculate".into(),
            description: "Calculate the result of a mathematical expression.".into(),
            schema: schemars::schema_for!(CalculateParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "cancel_pending_order".into(),
            description: "Cancel a pending order. Returns updated order details.".into(),
            schema: schemars::schema_for!(CancelPendingOrderParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "exchange_delivered_order_items".into(),
            description: "Exchange items in a delivered order.".into(),
            schema: schemars::schema_for!(ExchangeDeliveredOrderItemsParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "find_user_id_by_email".into(),
            description: "Look up a user ID by email address.".into(),
            schema: schemars::schema_for!(FindUserIdByEmailParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "find_user_id_by_name_zip".into(),
            description: "Look up a user ID by first name, last name, and ZIP code.".into(),
            schema: schemars::schema_for!(FindUserIdByNameZipParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "get_order_details".into(),
            description: "Get details of an order by order ID.".into(),
            schema: schemars::schema_for!(GetOrderDetailsParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "get_product_details".into(),
            description: "Get all variants and pricing for a product.".into(),
            schema: schemars::schema_for!(GetProductDetailsParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "get_item_details".into(),
            description: "Get details of a specific item variant.".into(),
            schema: schemars::schema_for!(GetItemDetailsParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "get_user_details".into(),
            description: "Get details of a user by user ID.".into(),
            schema: schemars::schema_for!(GetUserDetailsParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "list_all_product_types".into(),
            description: "List all available product type names.".into(),
            schema: empty_object_schema(),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "modify_pending_order_address".into(),
            description: "Modify the shipping address of a pending order.".into(),
            schema: schemars::schema_for!(ModifyPendingOrderAddressParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "modify_pending_order_items".into(),
            description: "Modify items in a pending order.".into(),
            schema: schemars::schema_for!(ModifyPendingOrderItemsParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "modify_pending_order_payment".into(),
            description: "Change the payment method for a pending order.".into(),
            schema: schemars::schema_for!(ModifyPendingOrderPaymentParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "modify_user_address".into(),
            description: "Update the address on file for a user.".into(),
            schema: schemars::schema_for!(ModifyUserAddressParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "return_delivered_order_items".into(),
            description: "Return items from a delivered order and issue a refund.".into(),
            schema: schemars::schema_for!(ReturnDeliveredOrderItemsParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "transfer_to_human_agents".into(),
            description: "Escalate the conversation to a human agent.".into(),
            schema: schemars::schema_for!(TransferToHumanAgentsParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
    ]
}

// ─── Airline params ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct BookReservationParams {
    /// User id of the passenger.
    pub user_id: String,
    /// Origin airport code.
    pub origin: String,
    /// Destination airport code.
    pub destination: String,
    /// Flight type: `one_way` or `round_trip`.
    pub flight_type: String,
    /// Cabin class: `basic_economy`, `economy`, `business`.
    pub cabin: String,
    /// List of flights to include (each has `flight_number` and `date`).
    pub flights: Vec<serde_json::Value>,
    /// List of passenger objects with `first_name`, `last_name`, `dob`.
    pub passengers: Vec<serde_json::Value>,
    /// Payment method id.
    pub payment_method_id: String,
    /// Total number of checked bags.
    pub total_baggages: u32,
    /// Number of non-free (charged) bags.
    pub nonfree_baggages: u32,
    /// Whether travel insurance is included: `yes` or `no`.
    pub insurance: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct CancelReservationParams {
    /// Reservation id.
    pub reservation_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct GetReservationDetailsParams {
    /// Reservation id.
    pub reservation_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct GetAirlineUserDetailsParams {
    /// User id.
    pub user_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct SearchDirectFlightParams {
    /// Origin airport code.
    pub origin: String,
    /// Destination airport code.
    pub destination: String,
    /// Departure date (YYYY-MM-DD).
    pub date: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct SearchOnestopFlightParams {
    /// Origin airport code.
    pub origin: String,
    /// Destination airport code.
    pub destination: String,
    /// Departure date (YYYY-MM-DD).
    pub date: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct SendCertificateParams {
    /// User id to send the certificate to.
    pub user_id: String,
    /// Dollar amount of the certificate.
    pub amount: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct UpdateReservationBaggagesParams {
    /// Reservation id.
    pub reservation_id: String,
    /// New total number of bags.
    pub total_baggages: u32,
    /// New number of non-free bags.
    pub nonfree_baggages: u32,
    /// Payment method id to charge extra bag fees.
    pub payment_method_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct UpdateReservationFlightsParams {
    /// Reservation id.
    pub reservation_id: String,
    /// Cabin class for the updated flights.
    pub cabin: String,
    /// New list of flights.
    pub flights: Vec<serde_json::Value>,
    /// Payment method id.
    pub payment_method_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct UpdateReservationPassengersParams {
    /// Reservation id.
    pub reservation_id: String,
    /// Updated passenger list.
    pub passengers: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct GetFlightStatusParams {
    /// Flight number.
    pub flight_number: String,
    /// Flight date (YYYY-MM-DD).
    pub date: String,
}

/// Return all tool definitions for the airline domain.
#[must_use]
pub fn airline_definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            id: "book_reservation".into(),
            description: "Book a new flight reservation for a user.".into(),
            schema: schemars::schema_for!(BookReservationParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "calculate".into(),
            description: "Calculate the result of a mathematical expression.".into(),
            schema: schemars::schema_for!(CalculateParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "cancel_reservation".into(),
            description: "Cancel an existing flight reservation and process refund.".into(),
            schema: schemars::schema_for!(CancelReservationParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "get_reservation_details".into(),
            description: "Get details of a reservation by reservation ID.".into(),
            schema: schemars::schema_for!(GetReservationDetailsParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "get_user_details".into(),
            description: "Get details of a user by user ID.".into(),
            schema: schemars::schema_for!(GetAirlineUserDetailsParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "list_all_airports".into(),
            description: "List all airports with their city, country, and code.".into(),
            schema: empty_object_schema(),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "search_direct_flight".into(),
            description: "Search for direct flights between two airports on a given date.".into(),
            schema: schemars::schema_for!(SearchDirectFlightParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "search_onestop_flight".into(),
            description: "Search for one-stop flights between two airports on a given date.".into(),
            schema: schemars::schema_for!(SearchOnestopFlightParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "send_certificate".into(),
            description: "Send a travel certificate to a user as compensation.".into(),
            schema: schemars::schema_for!(SendCertificateParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "transfer_to_human_agents".into(),
            description: "Escalate the conversation to a human agent.".into(),
            schema: schemars::schema_for!(TransferToHumanAgentsParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "update_reservation_baggages".into(),
            description: "Update the baggage allowance on a reservation.".into(),
            schema: schemars::schema_for!(UpdateReservationBaggagesParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "update_reservation_flights".into(),
            description: "Change the flights on an existing reservation.".into(),
            schema: schemars::schema_for!(UpdateReservationFlightsParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "update_reservation_passengers".into(),
            description: "Update passenger information on a reservation.".into(),
            schema: schemars::schema_for!(UpdateReservationPassengersParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
        ToolDef {
            id: "get_flight_status".into(),
            description: "Get the status of a flight by flight number and date.".into(),
            schema: schemars::schema_for!(GetFlightStatusParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        },
    ]
}
