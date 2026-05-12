#!/usr/bin/env python3
"""Generate Python RNS destination-hash fixtures for lxst.telephony."""

from __future__ import annotations

from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "common"))
from lxst_reference import import_rns_lxst, json_dump  # noqa: E402


def main() -> None:
    RNS, _LXST, _stubbed = import_rns_lxst()
    identity = RNS.Identity()
    destination_hash = RNS.Destination.hash_from_name_and_identity(
        "lxst.telephony",
        identity.hash,
    )

    json_dump(
        {
            "expanded_name": "lxst.telephony",
            "identity_hash": bytes(identity.hash).hex(),
            "destination_hash": bytes(destination_hash).hex(),
            "hash_from_name": bytes(destination_hash).hex(),
        }
    )


if __name__ == "__main__":
    main()
