// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`zeph_commands::SkillAccess`] implementation for [`Agent<C>`]: `/skill`, `/skills`, and
//! `/feedback`.
//!
//! Not to be confused with [`super::learning::skill_commands`], which holds the underlying
//! `handle_skill_command_as_string` helper this impl delegates to.
//!
//! [`Agent<C>`]: super::Agent

use std::future::Future;
use std::pin::Pin;

use zeph_commands::{CommandError, SkillAccess};

use super::Agent;
use super::command_macros::delegate_cmd;
use crate::channel::Channel;

impl<C: Channel + Send + 'static> SkillAccess for Agent<C> {
    // ----- /skill -----

    delegate_cmd!(handle_skill, handle_skill_command_as_string, args: &'a str => String);

    // ----- /skills -----

    delegate_cmd!(handle_skills, handle_skills_as_string, args: &'a str => String);

    // ----- /feedback -----

    delegate_cmd!(handle_feedback_command, handle_feedback_as_string, args: &'a str => String);
}
