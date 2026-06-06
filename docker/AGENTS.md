# Docker Guide

This directory contains container and image assets for Zeph.

- Keep images reproducible, minimal, and aligned with the documented install/run flows.
- Treat network exposure, credentials, user permissions, and base-image choices as security-sensitive.
- If Docker behavior changes, update the corresponding docs and examples.
- Avoid introducing container-specific behavior that diverges silently from the local CLI unless explicitly intended and documented.
