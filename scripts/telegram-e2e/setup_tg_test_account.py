#!/usr/bin/env python3
"""One-time setup: register a Telegram Test DC user account and save the session.

Run once before using telegram_e2e.py. Produces test_session.session (gitignored).

Usage:
    pip install telethon
    python3 scripts/telegram-e2e/setup_tg_test_account.py \\
        --api-id <API_ID> --api-hash <API_HASH> \\
        --phone +99966XXXXX --session .local/testing/test_session.session

Obtaining API credentials:
    Visit https://my.telegram.org → Log In → API development tools → Create app.
    Use Test DC credentials: set test mode in my.telegram.org before creating the app,
    or obtain a separate API_ID/API_HASH for test servers.

Test DC phone numbers:
    Any number in +99966XXXXX format works on Test DC (e.g. +9996612345).
    The OTP code is always the last 5 digits repeated (e.g. phone +9996612345 → OTP 12345).
    No real SIM needed — Test DC is an isolated Telegram server for developers.

Second account (for unauthorized-user scenario):
    Run again with a different phone and --session .local/testing/test_session2.session
"""

import argparse
import os
import sys

try:
    from telethon.sync import TelegramClient
except ImportError:
    print("telethon not installed. Run: pip install telethon", file=sys.stderr)
    sys.exit(1)

# Telegram Test DC server address
TEST_DC_HOST = "149.154.167.40"
TEST_DC_PORT = 443


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Create Telegram Test DC session for E2E testing"
    )
    parser.add_argument(
        "--api-id",
        type=int,
        default=int(os.environ.get("TG_API_ID", "0")) or None,
        required=not os.environ.get("TG_API_ID"),
        help="Telegram API ID from my.telegram.org",
    )
    parser.add_argument(
        "--api-hash",
        default=os.environ.get("TG_API_HASH"),
        required=not os.environ.get("TG_API_HASH"),
        help="Telegram API hash from my.telegram.org",
    )
    parser.add_argument(
        "--phone",
        required=True,
        help="Test DC phone number (+99966XXXXX format)",
    )
    parser.add_argument(
        "--session",
        default=os.environ.get("TG_SESSION_PATH", ".local/testing/test_session.session"),
        help="Path to save the session file (default: .local/testing/test_session.session)",
    )
    args = parser.parse_args()

    if not args.phone.startswith("+99966"):
        print(
            "WARNING: Test DC phone numbers use +99966XXXXX format.\n"
            "Using a real phone number will connect to production Telegram, not Test DC.",
            file=sys.stderr,
        )

    session_dir = os.path.dirname(args.session)
    if session_dir:
        os.makedirs(session_dir, exist_ok=True)

    # Strip .session suffix — Telethon appends it automatically
    session_name = args.session.removesuffix(".session")

    print(f"Connecting to Telegram Test DC ({TEST_DC_HOST}:{TEST_DC_PORT})...")
    print(f"Session will be saved to: {session_name}.session")

    client = TelegramClient(
        session_name,
        args.api_id,
        args.api_hash,
        server=(TEST_DC_HOST, TEST_DC_PORT),
    )

    # start() prompts for OTP interactively
    client.start(phone=args.phone)

    me = client.get_me()
    print(f"\nAuthenticated as: {me.first_name} (@{me.username or 'no username'})")
    print(f"Session saved: {session_name}.session")
    print("\nNext step: register a bot on Test DC via @BotFather (test server):")
    print("  1. In Telegram, go to @BotFather and /newbot")
    print("  2. Store the token: cargo run --features full -- vault set ZEPH_TELEGRAM_TEST_TOKEN '<TOKEN>'")
    print("  3. Run: python3 scripts/telegram-e2e/telegram_e2e.py --help")

    client.disconnect()


if __name__ == "__main__":
    main()
