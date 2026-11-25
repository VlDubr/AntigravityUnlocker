#!/usr/bin/env python3
"""Extract Antigravity OAuth credentials from a local IDE/CLI bundle.

Scans the Antigravity hub/CLI binary (``agy.exe`` / ``agy``) and the
``.gemini`` config directory for the embedded Google OAuth ``client_id`` /
``client_secret`` pair, and reads the OS keyring entry ``gemini:antigravity``
for the user's ``refresh_token``.

The client_id/client_secret are public-by-design desktop OAuth credentials
embedded in the shipped binary (not user secrets). The refresh_token IS a user
secret and is only read from the local keyring, never from the binary.

Usage:
    python extract-creds.py [--binary PATH] [--keyring] [--all-clients] [--json]

Defaults:
    --binary      auto-detected agy binary (~/.gemini/bin/agy[.exe])
    --keyring     also attempt to read gemini:antigravity from the OS keyring
    --all-clients include unrelated OAuth clients for diagnostics
    --json        emit machine-readable JSON instead of a human summary

Exit codes: 0 = found client pair, 1 = not found, 2 = binary missing.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import subprocess
import sys
from pathlib import Path

# Public desktop OAuth client_id format: <digits>-<alnum>.apps.googleusercontent.com
CLIENT_ID_RE = re.compile(
    rb"[0-9]{6,}-[a-z0-9]{20,}\.apps\.googleusercontent\.com"
)
# GOCSPX- prefixed client_secret. Google's standard secret is 28 chars after
# the prefix, but in a Go string table the next literal is concatenated with
# no separator (e.g. "...DAfcloudcode-pa"), so a boundary lookahead would drop
# real secrets. Capture the canonical 28-char prefix; longer runs are reported
# with a note that the tail may be an adjacent string.
CLIENT_SECRET_RE = re.compile(rb"GOCSPX-[A-Za-z0-9_\-]{28}")
CLIENT_SECRET_LONG_RE = re.compile(rb"GOCSPX-[A-Za-z0-9_\-]{29,}")

# Marker substrings used to annotate extracted credentials. The actual OAuth
# client_id / client_secret values are NOT stored in this repository — they are
# extracted from your local agy binary at runtime (see scan_binary). The
# consumer Antigravity desktop pair is the client_id found adjacent to
# "GetHubOAuthAccessToken" / "CLOUD_CODE_URL" in the binary's string table;
# use that pair for the default daily-cloudcode-pa upstream.
CONSUMER_ID_MARKER = "1071006060591-"   # Antigravity consumer desktop client
GEMINI_CLI_ID_MARKER = "681255809395-"  # Gemini CLI desktop client


def annotate_pair(client_id: str | None, client_secret: str | None) -> str:
    """Human-readable provenance for an extracted credential (no literals)."""
    if client_id and client_id.startswith(CONSUMER_ID_MARKER):
        return ("Antigravity consumer desktop pair (near GetHubOAuthAccessToken "
                "/ CLOUD_CODE_URL in agy). USE THIS for daily-cloudcode-pa.")
    if client_id and client_id.startswith(GEMINI_CLI_ID_MARKER):
        return "Gemini CLI desktop pair."
    if client_id:
        return "extracted from binary (other/enterprise client; pairing unconfirmed)"
    if client_secret:
        return "extracted from binary (secret; pairing unconfirmed)"
    return "unknown"


def default_binary() -> Path | None:
    home = Path.home()
    candidates = [
        home / ".gemini" / "bin" / ("agy.exe" if os.name == "nt" else "agy"),
        home / ".gemini" / "bin" / "agy",
    ]
    if os.name == "nt":
        local = Path(os.environ.get("LOCALAPPDATA", ""))
        candidates += [
            local / "Programs" / "Antigravity" / "resources" / "app" / "bin" / "agy.exe",
            local / "Programs" / "Antigravity IDE" / "resources" / "app" / "bin" / "agy.exe",
        ]
    for c in candidates:
        if c.is_file():
            return c
    return None


def scan_binary(path: Path) -> dict:
    """Stream the binary in chunks, collecting unique credential matches."""
    client_ids: set[str] = set()
    client_secrets: set[str] = set()
    long_runs: set[str] = set()
    chunk_size = 8 * 1024 * 1024
    overlap = 256  # keep tail so matches spanning chunk boundaries are caught
    tail = b""
    with path.open("rb") as f:
        while True:
            chunk = f.read(chunk_size)
            if not chunk:
                break
            buf = tail + chunk
            for m in CLIENT_ID_RE.finditer(buf):
                client_ids.add(m.group(0).decode("ascii", "replace"))
            for m in CLIENT_SECRET_LONG_RE.finditer(buf):
                run = m.group(0).decode("ascii", "replace")
                long_runs.add(run)
                # Canonical secret = first 28 chars after the prefix.
                client_secrets.add(run[:35])
            for m in CLIENT_SECRET_RE.finditer(buf):
                client_secrets.add(m.group(0).decode("ascii", "replace"))
            tail = buf[-overlap:]
    return {
        "binary": str(path),
        "binary_size": path.stat().st_size,
        "client_ids": sorted(client_ids),
        "client_secrets": sorted(client_secrets),
        "secret_long_runs": sorted(long_runs),
    }


def pair_candidates(ids: list[str], secrets: list[str],
                    long_runs: list[str]) -> list[dict]:
    """Annotate extracted credentials. Values come only from the binary scan.

    The consumer secret is identified structurally: in the agy Go string table
    its 28-char GOCSPX run is immediately followed by the literal
    ``cloudcode-pa`` (the next concatenated string). No credential literals are
    stored in this file.
    """
    consumer_id = next((c for c in ids if c.startswith(CONSUMER_ID_MARKER)), None)
    consumer_secret = None
    for run in long_runs:
        if run.endswith("cloudcode-pa") or "cloudcode-pa" in run[35:]:
            consumer_secret = run[:35]
            break
    out = []
    if consumer_id and consumer_secret:
        out.append({
            "client_id": consumer_id,
            "client_secret": consumer_secret,
            "source": annotate_pair(consumer_id, consumer_secret),
            "found_in_binary": True,
            "recommended": True,
        })
    for cid in ids:
        if cid == consumer_id:
            continue
        out.append({
            "client_id": cid, "client_secret": None,
            "source": annotate_pair(cid, None), "found_in_binary": True,
        })
    for sec in secrets:
        if sec == consumer_secret:
            continue
        out.append({
            "client_id": None, "client_secret": sec,
            "source": annotate_pair(None, sec), "found_in_binary": True,
        })
    return out


def _read_windows_generic_credential(target: str) -> str | None:
    """Read a generic credential blob via advapi32 CredReadW.

    Returns the decoded UTF-8 payload, '' for an empty blob, None if absent.
    """
    import ctypes
    from ctypes import wintypes

    adv = ctypes.windll.advapi32

    class CRED_ATTRIBUTE(ctypes.Structure):
        _fields_ = [("Keyword", wintypes.LPWSTR), ("Flags", wintypes.DWORD),
                    ("ValueSize", wintypes.DWORD), ("Value", ctypes.c_void_p)]

    class CRED(ctypes.Structure):
        _fields_ = [("Flags", wintypes.DWORD), ("Type", wintypes.DWORD),
                    ("TargetName", wintypes.LPWSTR), ("Comment", wintypes.LPWSTR),
                    ("LastWritten", wintypes.FILETIME),
                    ("CredentialBlobSize", wintypes.DWORD),
                    ("CredentialBlob", ctypes.c_void_p), ("Persist", wintypes.DWORD),
                    ("AttributeCount", wintypes.DWORD),
                    ("Attributes", ctypes.POINTER(CRED_ATTRIBUTE)),
                    ("TargetAlias", wintypes.LPWSTR), ("UserName", wintypes.LPWSTR)]

    PCRED = ctypes.POINTER(CRED)
    adv.CredReadW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                              ctypes.POINTER(PCRED)]
    adv.CredFree.argtypes = [ctypes.c_void_p]
    p = PCRED()
    if not adv.CredReadW(target, 1, 0, ctypes.byref(p)):
        return None
    c = p.contents
    n = c.CredentialBlobSize
    blob = ctypes.string_at(c.CredentialBlob, n) if n else b""
    adv.CredFree(p)
    return blob.decode("utf-8", "replace")


def read_keyring() -> dict | None:
    """Read gemini:antigravity generic credential from the OS keyring.

    Returns the parsed JSON payload {token:{access_token,refresh_token,...}}
    or None. The refresh_token is a USER SECRET; handle with care.
    """
    system = platform.system()
    try:
        if system == "Windows":
            # The entry is a *generic* credential (wincred / CredWriteW).
            # WinRT PasswordVault does NOT see generic credentials, so read
            # it with CredReadW directly.
            raw = _read_windows_generic_credential("gemini:antigravity")
            if raw is None:
                return None
        elif system == "Darwin":
            raw = subprocess.check_output(
                ["security", "find-generic-password", "-s", "gemini",
                 "-a", "antigravity", "-w"],
                text=True, stderr=subprocess.DEVNULL,
            ).strip()
        else:
            raw = subprocess.check_output(
                ["secret-tool", "lookup", "service", "gemini",
                 "username", "antigravity"],
                text=True, stderr=subprocess.DEVNULL,
            ).strip()
        if not raw:
            return None
        payload = json.loads(raw)
        tok = payload.get("token", {})
        return {
            "refresh_token": tok.get("refresh_token"),
            "access_token_prefix": (tok.get("access_token") or "")[:12] + "...",
            "expiry": tok.get("expiry"),
            "auth_method": payload.get("auth_method"),
            "is_placeholder": tok.get("refresh_token") == "proxy_managed_refresh_token",
        }
    except (subprocess.CalledProcessError, FileNotFoundError, json.JSONDecodeError):
        return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--binary", type=Path, default=None,
                    help="path to agy binary (auto-detected if omitted)")
    ap.add_argument("--keyring", action="store_true",
                    help="also read gemini:antigravity from OS keyring")
    ap.add_argument("--all-clients", action="store_true",
                    help="show unrelated OAuth clients found in the binary")
    ap.add_argument("--json", action="store_true", help="emit JSON")
    args = ap.parse_args()

    binary = args.binary or default_binary()
    if binary is None or not binary.is_file():
        print("ERROR: agy binary not found. Pass --binary PATH.", file=sys.stderr)
        return 2

    scan = scan_binary(binary)
    all_pairs = pair_candidates(
        scan["client_ids"], scan["client_secrets"], scan["secret_long_runs"]
    )
    pairs = all_pairs if args.all_clients else [
        pair for pair in all_pairs if pair.get("recommended")
    ]
    result = {
        "binary": scan["binary"],
        "binary_size": scan["binary_size"],
        "pairs": pairs,
    }
    if args.all_clients:
        result.update({
            "client_ids_found": scan["client_ids"],
            "client_secrets_found": scan["client_secrets"],
            "secret_long_runs": scan["secret_long_runs"],
        })
    if args.keyring:
        result["keyring"] = read_keyring()

    if args.json:
        print(json.dumps(result, indent=2, ensure_ascii=False))
    else:
        print(f"Binary: {scan['binary']} ({scan['binary_size']:,} bytes)")
        if args.all_clients:
            print(f"client_id matches: {len(scan['client_ids'])}")
            for cid in scan["client_ids"]:
                print(f"  {cid}")
            print(f"client_secret matches: {len(scan['client_secrets'])}")
            for sec in scan["client_secrets"]:
                print(f"  {sec}")
            print("\nCandidate pairs:")
        else:
            print("\nAntigravity 2.0 OAuth credentials:")
        for p in pairs:
            print(f"  client_id     = {p.get('client_id')}")
            print(f"  client_secret = {p.get('client_secret')}")
            if args.all_clients:
                print(f"  source        = {p.get('source')}")
            print()
        if args.keyring:
            kr = result.get("keyring")
            if kr is None:
                print("Keyring: gemini:antigravity not readable "
                      "(log into Antigravity natively first).")
            elif kr.get("is_placeholder"):
                print("Keyring: PLACEHOLDER token (proxy_managed_refresh_token). "
                      "The Antigravity Client overwrote it. Extract BEFORE "
                      "launching the client, or re-login natively.")
            else:
                print(f"Keyring refresh_token: {kr.get('refresh_token')}")
                print(f"Keyring expiry: {kr.get('expiry')}")
        if not pairs:
            print("No known OAuth pair found in binary.")
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())