"""Session-bound Python callable inventory for optional native plugins.

The kernel owner preimports ``PluginNamespace(session)`` as ``plugin``, then
awaits ``plugin.refresh()`` when the native inventory changes. No discovery or
I/O runs at import time. Commands and hooks are not Python exports.

Wire contract (over the existing ``rlm.host_request`` transport):
* ``plugin.catalog`` receives ``{session}`` and returns
  ``{session, revision, exports: [{plugin, generation, name, description, mode}]}``.
* ``plugin.invoke`` receives ``{session, generation, plugin, export, input}``.
  Only keyword arguments are accepted; ``input`` is their JSON object, not an
  args/kwargs envelope. The unwrapped host result is returned unchanged.
* Modes are ``unary`` and ``streaming``. Streaming handles explicitly raise
  ``NotImplementedError``; this bridge never discards updates to fake unary I/O.

Refresh validates a whole catalog before one atomic publication. Older replies
cannot overwrite newer published refreshes or revisions. Saved callables retain
their session and generation; the host remains authoritative for retirement.
"""

from __future__ import annotations

import json
import keyword
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from types import MappingProxyType
from typing import Any, Mapping

__all__ = ["PluginNamespace", "PluginExport"]

Transport = Callable[[str, dict[str, Any]], Awaitable[Any]]


async def _native_request(kind: str, payload: dict[str, Any]) -> Any:
    from . import host_request

    return await host_request(kind, payload)


def _name(value: Any) -> bool:
    return (
        isinstance(value, str) and value.isidentifier()
        and not value.startswith("_") and not keyword.iskeyword(value)
    )


def _json_keys(value: Any) -> None:
    # json.dumps rejects non-JSON values and cycles, but silently coerces some
    # object keys. Reject those rather than changing a caller's input keys.
    if isinstance(value, dict):
        for key, child in value.items():
            if not isinstance(key, str):
                raise TypeError("plugin input object keys must be strings")
            _json_keys(child)
    elif isinstance(value, (list, tuple)):
        for child in value:
            _json_keys(child)


@dataclass(frozen=True)
class PluginExport:
    """Immutable declared target. Its identity never follows a reload."""

    session: str
    plugin: str
    generation: str
    name: str
    description: str
    mode: str


@dataclass(frozen=True)
class _CallableExport:
    descriptor: PluginExport
    _transport: Transport

    @property
    def __doc__(self) -> str:
        return self.descriptor.description

    async def __call__(self, **kwargs: Any) -> Any:
        if self.descriptor.mode == "streaming":
            raise NotImplementedError("Python plugin exports do not support streaming yet")
        # Snapshot input before yielding; the transport only sees structured JSON.
        encoded = json.dumps(kwargs, ensure_ascii=False, allow_nan=False)
        _json_keys(kwargs)
        target = self.descriptor
        return await self._transport("plugin.invoke", {
            "session": target.session,
            "generation": target.generation,
            "plugin": target.plugin,
            "export": target.name,
            "input": json.loads(encoded),
        })


class _PluginView:
    def __init__(self, owner: PluginNamespace, name: str) -> None:
        self._owner = owner
        self._name = name

    def __getattr__(self, name: str) -> _CallableExport:
        try:
            return self._owner._exports[self._name][name]
        except KeyError:
            raise AttributeError(name) from None

    def __dir__(self) -> list[str]:
        return sorted(self._owner._exports.get(self._name, {}))


class PluginNamespace:
    """Current exports for one explicitly supplied session, with an injectable transport.

    ``inventory`` returns immutable descriptors. An absent/disabled plugin has no
    attribute or inventory entry. A previously saved plugin view follows current
    membership, but a previously saved callable always invokes its old target.
    Host/transport exceptions propagate unchanged. No local files are written.
    """

    def __init__(self, session: str, *, transport: Transport = _native_request) -> None:
        if not isinstance(session, str) or not session:
            raise ValueError("plugin namespace requires a non-empty session identity")
        self._session = session
        self._transport = transport
        self._exports: Mapping[str, Mapping[str, _CallableExport]] = MappingProxyType({})
        self._revision = -1
        self._serial = 0
        self._published = 0

    @property
    def session(self) -> str:
        return self._session

    @property
    def revision(self) -> int | None:
        return self._revision if self._revision >= 0 else None

    @property
    def inventory(self) -> tuple[PluginExport, ...]:
        return tuple(
            exports[name].descriptor
            for plugin, exports in sorted(self._exports.items())
            for name in sorted(exports)
        )

    def __getattr__(self, name: str) -> _PluginView:
        if name not in self._exports:
            raise AttributeError(name)
        return _PluginView(self, name)

    def __dir__(self) -> list[str]:
        return sorted({"refresh", "inventory", "session", "revision", *self._exports})

    async def refresh(self) -> bool:
        """Fetch and validate a catalog; return whether this reply was published.

        A malformed or wrong-session reply raises ValueError without changing
        the inventory. An out-of-order reply returns False. Revisions must be
        nonnegative integers and equal revisions must describe equal inventories.
        """
        self._serial += 1
        serial = self._serial
        catalog = await self._transport("plugin.catalog", {"session": self._session})
        if not isinstance(catalog, dict) or catalog.get("session") != self._session:
            raise ValueError("plugin catalog session mismatch or malformed catalog")
        revision = catalog.get("revision")
        entries = catalog.get("exports")
        if type(revision) is not int or revision < 0 or not isinstance(entries, list):
            raise ValueError("plugin catalog requires a revision and exports list")
        replacement: dict[str, dict[str, _CallableExport]] = {}
        generations: dict[str, str] = {}
        for entry in entries:
            if not isinstance(entry, dict):
                raise ValueError("malformed plugin export descriptor")
            plugin, name = entry.get("plugin"), entry.get("name")
            generation = entry.get("generation")
            description, mode = entry.get("description"), entry.get("mode")
            if (
                not _name(plugin) or not _name(name)
                or plugin in {"refresh", "inventory", "session", "revision"}
                or not isinstance(generation, str) or not generation
                or not isinstance(description, str) or mode not in ("unary", "streaming")
                or entry.get("session", self._session) != self._session
                or entry.get("kind", "export") != "export"
            ):
                raise ValueError("malformed or wrong-session plugin export descriptor")
            exports = replacement.setdefault(plugin, {})
            if name in exports or generations.get(plugin, generation) != generation:
                raise ValueError("duplicate export or mixed plugin generations")
            generations[plugin] = generation
            exports[name] = _CallableExport(PluginExport(
                self._session, plugin, generation, name, description, mode,
            ), self._transport)
        # There are no await points between validation and publication.
        if serial < self._published or revision < self._revision:
            return False
        current = {p: dict(e) for p, e in self._exports.items()}
        if revision == self._revision and replacement != current:
            raise ValueError("plugin catalog changed without a new revision")
        self._exports = MappingProxyType({
            plugin: MappingProxyType(exports) for plugin, exports in replacement.items()
        })
        self._revision = revision
        self._published = serial
        return True
