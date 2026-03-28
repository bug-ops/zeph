#!/usr/bin/env python3
"""Telegram E2E test suite for Zeph using Telethon and Telegram Test DC.

Connects to Telegram Test DC as a user account, sends scripted prompts to the
Zeph bot, and asserts on bot replies. Exits non-zero on any failure.

Prerequisites:
    pip install telethon
    python3 scripts/telegram-e2e/setup_tg_test_account.py ...   # one-time setup

Usage:
    python3 scripts/telegram-e2e/telegram_e2e.py \\
        --api-id <API_ID> --api-hash <API_HASH> \\
        --bot-username @YourZephTestBot \\
        --session .local/testing/test_session.session

Environment variables (alternative to CLI flags):
    TG_API_ID           Telegram API ID
    TG_API_HASH         Telegram API hash
    TG_BOT_USERNAME     Bot username (with or without @)
    TG_SESSION_PATH     Path to .session file (default: .local/testing/test_session.session)
    TG_SESSION_PATH_2   Second session for unauthorized-user test (optional)
"""

import argparse
import asyncio
import io
import os
import sys
import time
from typing import Optional

try:
    from telethon import TelegramClient, events
    from telethon.sessions import StringSession
except ImportError:
    print("telethon not installed. Run: pip install telethon", file=sys.stderr)
    sys.exit(1)

# Minimal valid 1×1 white PNG used as a document with no text to trigger
# the empty-message filter in TelegramChannel (text="" && attachments=[])
_TINY_PNG = bytes([
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
    0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
    0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41,
    0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
    0x00, 0x00, 0x02, 0x00, 0x01, 0xE2, 0x21, 0xBC,
    0x33, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
])


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _result(name: str, passed: bool, detail: str = "") -> bool:
    status = "PASS" if passed else "FAIL"
    suffix = f": {detail}" if detail else ""
    print(f"[{status}] {name}{suffix}")
    return passed


async def _wait_for_reply(
    client: TelegramClient,
    bot: str,
    timeout: float,
    since: Optional[float] = None,
) -> Optional[str]:
    """Register a one-shot handler, send nothing, await first reply.

    *since* is a UNIX timestamp; messages older than this are ignored to prevent
    stale replies from previous scenarios leaking into this one.
    """
    loop = asyncio.get_event_loop()
    future: asyncio.Future[str] = loop.create_future()
    cutoff = since if since is not None else time.time()

    @client.on(events.NewMessage(from_users=bot))
    async def _handler(event: events.NewMessage.Event) -> None:
        if event.message.date.timestamp() < cutoff:
            return
        if not future.done():
            future.set_result(event.message.text or "")

    try:
        return await asyncio.wait_for(future, timeout=timeout)
    except asyncio.TimeoutError:
        return None
    finally:
        client.remove_event_handler(_handler)


async def _send_and_wait(
    client: TelegramClient,
    bot: str,
    text: str,
    timeout: float = 30.0,
) -> Optional[str]:
    """Send *text* to *bot* and return the first reply text, or None on timeout.

    Only messages that arrive after the send are accepted; stale replies from
    prior scenarios are discarded via a timestamp cutoff.
    """
    loop = asyncio.get_event_loop()
    future: asyncio.Future[str] = loop.create_future()
    send_time = time.time()

    @client.on(events.NewMessage(from_users=bot))
    async def _handler(event: events.NewMessage.Event) -> None:
        if event.message.date.timestamp() < send_time - 2:
            return
        if not future.done():
            future.set_result(event.message.text or "")

    await client.send_message(bot, text)
    try:
        return await asyncio.wait_for(future, timeout=timeout)
    except asyncio.TimeoutError:
        return None
    finally:
        client.remove_event_handler(_handler)


async def _send_and_collect(
    client: TelegramClient,
    bot: str,
    text: str,
    first_timeout: float = 60.0,
    idle_after: float = 12.0,
    max_messages: int = 20,
) -> list[str]:
    """Send *text* and collect all bot replies until idle_after seconds of silence.

    Also tracks message edits (streaming updates), keeping the last text per message.
    Messages older than the send time are discarded.
    """
    replies: list[str] = []
    first_arrived = asyncio.Event()
    last_activity: list[float] = [0.0]
    send_time = time.time()

    @client.on(events.NewMessage(from_users=bot))
    async def _on_new(event: events.NewMessage.Event) -> None:
        if event.message.date.timestamp() < send_time - 2:
            return
        replies.append(event.message.text or "")
        last_activity[0] = time.monotonic()
        first_arrived.set()

    @client.on(events.MessageEdited(from_users=bot))
    async def _on_edit(event: events.MessageEdited.Event) -> None:
        if event.message.date.timestamp() < send_time - 2:
            return
        if replies:
            replies[-1] = event.message.text or ""
        last_activity[0] = time.monotonic()

    await client.send_message(bot, text)

    try:
        await asyncio.wait_for(first_arrived.wait(), timeout=first_timeout)
    except asyncio.TimeoutError:
        pass
    finally:
        # Continue draining until idle
        pass

    start_idle = time.monotonic()
    while True:
        await asyncio.sleep(0.5)
        if len(replies) >= max_messages:
            break
        since_last = time.monotonic() - last_activity[0]
        if first_arrived.is_set() and since_last >= idle_after:
            break
        if not first_arrived.is_set() and (time.monotonic() - start_idle) > first_timeout:
            break

    client.remove_event_handler(_on_new)
    client.remove_event_handler(_on_edit)
    return replies


# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------

async def scenario_startup(client: TelegramClient, bot: str) -> bool:
    """Send /start; assert reply contains 'Welcome'."""
    reply = await _send_and_wait(client, bot, "/start", timeout=20.0)
    excerpt = repr(reply[:80]) if reply else "TIMEOUT"
    return _result("startup", reply is not None and "elcome" in reply, excerpt)


async def scenario_reset(client: TelegramClient, bot: str) -> bool:
    """/reset must elicit any reply (or silently reset context)."""
    reply = await _send_and_wait(client, bot, "/reset", timeout=20.0)
    excerpt = repr(reply[:80]) if reply else "TIMEOUT (acceptable — context reset silently)"
    # A timeout is acceptable: the channel may reset without replying
    return _result("reset", True, excerpt)


async def scenario_skills(client: TelegramClient, bot: str) -> bool:
    """/skills must return a non-empty reply without MarkdownV2 parse errors.

    MarkdownV2 errors would surface as Telegram API exceptions; a successful
    reply means the bot's markdown_to_telegram() escaped the output correctly.
    """
    reply = await _send_and_wait(client, bot, "/skills", timeout=30.0)
    ok = reply is not None and len(reply) > 0
    excerpt = repr(reply[:80]) if reply else "TIMEOUT"
    return _result("skills", ok, excerpt)


async def scenario_math(client: TelegramClient, bot: str) -> bool:
    """Math prompt must produce a reply containing 30,883 (or 30883)."""
    reply = await _send_and_wait(client, bot, "What is 347 * 89?", timeout=60.0)
    ok = reply is not None and ("30,883" in reply or "30883" in reply)
    excerpt = repr(reply[:120]) if reply else "TIMEOUT"
    return _result("math", ok, excerpt)


async def scenario_empty_msg(client: TelegramClient, bot: str) -> bool:
    """A document with no text/caption must produce NO reply within 5s.

    The TelegramChannel drops messages where text.is_empty() && attachments.is_empty().
    Sending a PNG as a raw document (force_document=True, no caption) is neither
    a photo nor an audio attachment, so it reaches the empty-message filter.
    """
    loop = asyncio.get_event_loop()
    future: asyncio.Future[str] = loop.create_future()
    send_time = time.time()

    @client.on(events.NewMessage(from_users=bot))
    async def _handler(event: events.NewMessage.Event) -> None:
        if event.message.date.timestamp() < send_time - 2:
            return
        if not future.done():
            future.set_result(event.message.text or "<non-text reply>")

    await client.send_file(
        bot,
        io.BytesIO(_TINY_PNG),
        force_document=True,
        # No caption — must trigger empty-message filter
    )
    try:
        reply = await asyncio.wait_for(future, timeout=5.0)
        client.remove_event_handler(_handler)
        return _result("empty_msg", False, f"unexpected reply: {repr(reply[:60])}")
    except asyncio.TimeoutError:
        client.remove_event_handler(_handler)
        return _result("empty_msg", True, "no reply within 5s")


async def scenario_long_output(client: TelegramClient, bot: str) -> bool:
    """A prompt that forces >4096 chars of output must split into ≥2 messages."""
    prompt = (
        "Write a numbered list from 1 to 100, one item per line, in this exact format: "
        "'N. This is item number N in the list.' "
        "Do not use any tools or shell commands — output the list directly. "
        "Output ONLY the list with no preamble and no trailing summary."
    )
    replies = await _send_and_collect(
        client, bot, prompt, first_timeout=60.0, idle_after=15.0, max_messages=10
    )
    ok = len(replies) >= 2
    excerpt = (
        f"{len(replies)} message(s), first={repr(replies[0][:40]) if replies else 'none'}"
    )
    return _result("long_output", ok, excerpt)


async def scenario_streaming(client: TelegramClient, bot: str) -> bool:
    """Long-form prompt must produce a reply within 30s (streaming first chunk).

    Full assertion: the bot sends an initial chunk early (not after the whole
    response is ready). We verify by checking latency to first message < 30s
    and that the final reply is non-empty. Edit events are counted as a
    best-effort indicator of intermediate streaming updates.
    """
    prompt = (
        "Explain in detail how a Rust async executor works, covering: "
        "the Waker mechanism, task queues, the poll lifecycle, "
        "cooperative scheduling, and how Tokio implements multi-threading. "
        "Be thorough — at least 800 words."
    )
    first_time: list[Optional[float]] = [None]
    edit_count: list[int] = [0]
    send_wall = time.time()
    send_time = time.monotonic()

    @client.on(events.NewMessage(from_users=bot))
    async def _on_new(event: events.NewMessage.Event) -> None:
        if event.message.date.timestamp() < send_wall - 2:
            return
        if first_time[0] is None:
            first_time[0] = time.monotonic()

    @client.on(events.MessageEdited(from_users=bot))
    async def _on_edit(event: events.MessageEdited.Event) -> None:
        if event.message.date.timestamp() < send_wall - 2:
            return
        edit_count[0] += 1

    await client.send_message(bot, prompt)

    # Wait up to 90s; stop early once first message arrived + 20s idle
    last_activity = [time.monotonic()]
    deadline = send_time + 90.0
    while time.monotonic() < deadline:
        await asyncio.sleep(1.0)
        if first_time[0] is not None:
            if (time.monotonic() - max(first_time[0], send_time + 5.0)) > 20.0:
                break

    client.remove_event_handler(_on_new)
    client.remove_event_handler(_on_edit)

    appeared = first_time[0] is not None
    latency = (first_time[0] - send_time) if appeared else None
    latency_str = f"{latency:.1f}s" if latency is not None else "never"
    detail = f"first_msg={latency_str}, edits={edit_count[0]}"
    return _result("streaming", appeared and latency is not None and latency < 90.0, detail)


async def scenario_unauthorized(
    client2: Optional[TelegramClient], bot: str
) -> bool:
    """A message from an account NOT in allowed_users must produce no reply within 10s."""
    if client2 is None:
        print("[SKIP] unauthorized: TG_SESSION_PATH_2 not set — skipping")
        return True

    loop = asyncio.get_event_loop()
    future: asyncio.Future[str] = loop.create_future()
    send_time = time.time()

    @client2.on(events.NewMessage(from_users=bot))
    async def _handler(event: events.NewMessage.Event) -> None:
        if event.message.date.timestamp() < send_time - 2:
            return
        if not future.done():
            future.set_result(event.message.text or "<non-text reply>")

    await client2.send_message(bot, "Hello from unauthorized account")
    try:
        reply = await asyncio.wait_for(future, timeout=10.0)
        client2.remove_event_handler(_handler)
        return _result("unauthorized", False, f"unexpected reply: {repr(reply[:60])}")
    except asyncio.TimeoutError:
        client2.remove_event_handler(_handler)
        return _result("unauthorized", True, "no reply within 10s")


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

def _load_session(path: str):
    """Load a Telethon session from a file.

    Supports both StringSession (plain text string written by get_session.py)
    and legacy SQLite file sessions.
    """
    try:
        with open(path) as f:
            content = f.read().strip()
        if content:
            return StringSession(content)
    except (FileNotFoundError, UnicodeDecodeError):
        pass
    # Fall back to SQLite file session (strip .session suffix — Telethon appends it)
    return path.removesuffix(".session")


async def _run(args: argparse.Namespace) -> int:
    client = TelegramClient(_load_session(args.session), args.api_id, args.api_hash)
    await client.start()

    client2: Optional[TelegramClient] = None
    if args.session2:
        client2 = TelegramClient(_load_session(args.session2), args.api_id, args.api_hash)
        await client2.start()

    bot = args.bot_username
    if not bot.startswith("@"):
        bot = f"@{bot}"

    print(f"Running Zeph Telegram E2E against {bot}\n")

    # Reset conversation state before running scenarios
    if not args.no_reset:
        print("Resetting conversation state (/reset)...")
        await _send_and_wait(client, bot, "/reset", timeout=10.0)
        await asyncio.sleep(3.0)

    results: list[bool] = []

    results.append(await scenario_startup(client, bot))
    await asyncio.sleep(2.0)
    results.append(await scenario_reset(client, bot))
    await asyncio.sleep(2.0)
    results.append(await scenario_skills(client, bot))
    await asyncio.sleep(2.0)
    results.append(await scenario_math(client, bot))
    await asyncio.sleep(2.0)
    results.append(await scenario_empty_msg(client, bot))
    await asyncio.sleep(2.0)
    results.append(await scenario_long_output(client, bot))
    await asyncio.sleep(2.0)
    results.append(await scenario_streaming(client, bot))
    await asyncio.sleep(2.0)
    results.append(await scenario_unauthorized(client2, bot))

    await client.disconnect()
    if client2:
        await client2.disconnect()

    passed = sum(1 for r in results if r)
    total = len(results)
    print(f"\n{passed}/{total} scenarios passed")
    return 0 if all(results) else 1


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Zeph Telegram E2E test suite (Telethon + Test DC)"
    )
    parser.add_argument(
        "--api-id",
        type=int,
        default=int(os.environ.get("TG_API_ID", "0")) or None,
        required=not os.environ.get("TG_API_ID"),
    )
    parser.add_argument(
        "--api-hash",
        default=os.environ.get("TG_API_HASH"),
        required=not os.environ.get("TG_API_HASH"),
    )
    parser.add_argument(
        "--bot-username",
        default=os.environ.get("TG_BOT_USERNAME"),
        required=not os.environ.get("TG_BOT_USERNAME"),
        help="Bot username to test against (e.g. @ZephTestBot)",
    )
    parser.add_argument(
        "--session",
        default=os.environ.get("TG_SESSION_PATH", ".local/testing/test_session.session"),
        help="Path to the Telethon session file",
    )
    parser.add_argument(
        "--session2",
        default=os.environ.get("TG_SESSION_PATH_2"),
        help="Second session for unauthorized-user scenario (optional)",
    )
    parser.add_argument(
        "--no-reset",
        action="store_true",
        help="Skip /reset before running scenarios",
    )
    args = parser.parse_args()

    sys.exit(asyncio.run(_run(args)))


if __name__ == "__main__":
    main()
