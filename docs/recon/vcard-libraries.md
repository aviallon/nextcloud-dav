# vCard parsing / validation / serialization: crate reconnaissance

Status: research, 2026-09-17. Author: worker agent (research task).
Target: replace the hand-rolled parser in `nextcloud-dav/src/vcard.rs` (read-only
CardDAV sidecar) with a real library, and answer what a write-capable CardDAV
server would need.

All version/date/license/download claims below were read from crates.io,
docs.rs, GitHub and the crate sources on 2026-09-17. Behavioural claims marked
**[measured]** come from a throwaway crate at `/tmp/vcardtest` built with
`nix shell nixpkgs#cargo …` (vcard-rs 0.4.0, calcard 0.3.14, vcard4 0.7.3,
vcard 0.5.0).

---

## 0. Scope and constraints

- The sidecar is **read-only**: it parses `carddata` blobs read from the
  Nextcloud DB to evaluate `addressbook-query` filters
  (`src/xml/filter.rs`, a port of `Sabre\CardDAV\Plugin::validateFilters()`),
  to answer `has-photo`, and to reproduce
  `CardDavBackend::readBlob()` (`src/vcard.rs::filter_read_blob`).
  It does not accept `PUT`, so it does not itself validate uploads today.
- The task asks for a library that *could* back a write path too: reject what
  sabre rejects, store byte-stable data, be safe against hostile input, and
  match `sabre/vobject` as closely as practical.
- Constraint: AGPL-3.0-or-later sidecar; all candidates below are MIT/Apache/
  ISC, i.e. compatible. No GPL-incompatible licenses found.

---

## 1. What sabre/vobject actually does (the behaviour to match)

Reference: `3rdparty/sabre/dav/lib/CardDAV/Plugin.php::validateVCard()` (~L308),
`3rdparty/sabre/vobject/lib/Component/VCard.php::validate()` (L221),
`Component.php::validate()` (L571), `Property.php::validate()` (L529),
`Property/Text.php::validate()` (L370), `StringUtil.php`.

Flow on `PUT` into an address book:

1. If the body starts with `[`, parse as **jCard** (`Reader::readJson`) and
   immediately re-serialize to vCard text (`$data = $vobj->serialize()`,
   `modified = true`). Otherwise `Reader::read()` (MimeDir).
2. `ParseException` → HTTP **415 Unsupported Media Type**.
3. `$vobj->name !== 'VCARD'` → **415**.
4. `$options = PROFILE_CARDDAV`; unless the request sends
   `Prefer: handling=strict`, `REPAIR` is OR-ed in.
5. `$vobj->validate($options)` returns messages with `level` 1/2/3:
   - **level 3** → **415** `Validation error in vCard: …` (reject).
   - **level 1** = "was repaired", sets `modified = true`.
   - **level 2** = warning only.
6. If *any* message was produced, the object is **re-serialized**
   (`$data = $vobj->serialize()`), and if the bytes changed the ETag is not
   returned as-is (`modified = true`). A clean card is stored byte-for-byte.
7. `$vobj->destroy()` (breaks circular refs for GC).

CardDAV-specific rules (VCard.php::validate):

- `VERSION` must be exactly one of `2.1`, `3.0`, `4.0` → otherwise level 3
  (REPAIR sets `4.0`).
- **vCard 2.1 is level 3 under `PROFILE_CARDDAV`** — "CardDAV servers are not
  allowed to accept vCard 2.1".
- `UID` missing → level 3 under `PROFILE_CARDDAV` ("vCards on CardDAV servers
  MUST have a UID property"); REPAIR generates a UUID and downgrades to level 1.
- `FN` must appear **exactly once** → level 3. REPAIR (only when FN is absent)
  derives it from `N` (`value[1] + ' ' + value[0]`, else `value[0]`), else
  `ORG`, else `NICKNAME`, else `EMAIL`, and downgrades to level 1.
- Per-property cardinality table (`getValidationRules()`): `VERSION` = exactly 1,
  `N`/`BDAY`/… = `?` (at most 1), most = `*`, `FN` handled separately.
  Duplicate `?` properties with identical values are de-duplicated under REPAIR
  (level 1).
- `Component.php` also enforces defaults for `1`/`+` rules.

Property-level rules (Property.php / Text.php / StringUtil.php):

- Value must be valid UTF-8 and contain no control characters
  `[\x00-\x08\x0B-\x0C\x0E-\x1F\x7F]` → level 3. REPAIR converts ISO-8859-1 →
  UTF-8 and strips control characters → level 1.
- Property **name** must match `^[A-Z0-9-]+$` → level 3; REPAIR upper-cases,
  turns `_` into `-` and strips the rest.
- `ENCODING` handling is version-specific:
  - vCard 4.0: `ENCODING` present at all → level 3.
  - vCard 2.1: allowed `QUOTED-PRINTABLE`, `BASE64`, `8BIT`.
  - vCard 3.0: allowed `B`; REPAIR turns `BASE64` into `B` (level 1).
- `Text` enforces per-property minimum value counts (e.g. `N`/`ADR`/`ORG`
  component counts).

Nextcloud adds on top:

- `DAV\StringUtil::ensureUTF8($cardData)` is applied in
  `CardDAV/Card.php::put()` (L92) and `AddressBook.php` (L142): if the bytes
  are not UTF-8 but are ISO-8859-1, transcode to UTF-8; otherwise pass through.
- Size cap: `CardDavValidatePlugin::beforePut()` (L34) reads
  `card_size_limit` (default **5242880**) and throws **403 Forbidden** if
  `CONTENT_LENGTH` exceeds it.
- UID uniqueness inside an address book via `getCardByUid()`
  (`CardDavBackend.php` L556/L657) → RFC 6352 `no-uid-conflict` (409).
- Indexed search columns are truncated with `mb_strcut($value, 0, 254)`
  (`CardDavBackend.php` L1448) over `INDEXED_PROPERTIES` (L47).

So the honest target is: **parse leniently, validate strictly, store the
original bytes when nothing was repaired, and re-serialize only when you
actually repaired.** That is exactly what sabre does.

---

## 2. Candidate crates

### Comparison table

| Crate | Latest | Released | License | Repo / maintainer | Stars | All-time dl | vCard versions | jCard | Strict validation | Byte-stable round-trip | Dep weight (crate tree) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| **vcard-rs** (lib name `vcard`) | 0.4.0 | 2026-08-31 | MIT OR Apache-2.0 | pimalaya/vcard (soywod) | 1 | 254 | 2.1, 3.0, 4.0, RFC 9554 | ✅ (RFC 7095) | ✅ per-property spec: cardinality, value kind, allowed params, version | ✅ **byte-exact** incl. folds/blank lines/bare LF/QP soft breaks **[measured]** | light: memchr, base64, encoding_rs, quoted_printable (24 nodes) |
| **calcard** | 0.3.14 | 2026-09-15 | Apache-2.0 OR MIT | stalwartlabs/calcard | 75 | 52,525 | 2.1, 3.0, 4.0 (+JSCalendar/JSContact) | ❌ (JSContact instead) | ❌ none — lenient by design | ❌ canonical re-emit; `VERSION:5.0` silently becomes `4.0` **[measured]** | heavier: chrono-tz (tz DB), mail-builder, mail-parser, ahash (36 nodes) |
| **vcard4** | 0.7.3 | 2026-02-07 | MIT OR Apache-2.0 | tmpfs/vcard4 | 4 | 49,674 | **4.0 only**; a 3.0 card is coerced to 4.0 **[measured]**; 2.1 errors | ❌ | parse errors only (RFC 6350) | ❌ re-serializes canonically | small; zeroize on by default |
| **vcard** (magiclen) | 0.5.0 | 2026-07-11 | MIT | magiclen/vcard | 22 | 53,882 | **4.0 only**; rejects 2.1/3.0 (`UnsupportedVersion`) **[measured]** | ❌ | `validate()` via `validators` crate | ❌ canonical | pulls `validators`, `url`/idna, `phonenumber`, ICU |
| **vcard_parser** | 0.2.3 | 2026-05-30 | MIT | kenianbei/vcard_parser | 7 | 26,908 | **4.0 only** (RFC 6350) | ❌ | parse + RFC 6350 validation | ❌ canonical | nom-based |
| **caldata** | 0.17.1 | 2026-09-06 | Apache-2.0 | lennart-k/caldata-rs | 2 | 1,988 | ical+vcard content model (used by **rustical**) | ❌ | ❌ none (parser) | ❌ canonical | thiserror, chrono, regex, itertools, derive_more |
| **ical_vcard** | 0.5.0 | 2026-08-14 | MIT OR Apache-2.0 | codeberg.org/darkfire/ical_vcard | — | 20,648 | content-line layer, version-agnostic | ❌ | ❌ low-level | content lines exact; **has `max_line_length` DoS bound**; RFC 6868 | tiny, no unsafe |
| **vparser** | 1.2.1 | 2026-04-10 | ISC | vdirsyncer-rs (sr.ht) | — | 12,011 | content-line layer | ❌ | ❌ non-validating by design | — | small |
| **jcard** | 0.4.3 | 2026-09-10 | MIT OR Apache-2.0 | ticpu/jcard | 0 | 264 | jCard only (serde types) | ✅ | ❌ | — | serde |
| **vobject** | 0.9.0 | 2026-04-01 | MIT | untitaker/rust-vobject | 17 | 42,729 | minimal vCard builder/reader; **"property encodings are missing"** per README | ❌ | ❌ | ❌ | chrono optional, thiserror |
| **ical** (Peltoche) | 0.11.0 | 2024-03-13 | non-standard | Peltoche/ical-rs — **repo archived** | 110 | 1,522,755 | iCalendar only | ❌ | ❌ | — | — |
| **icalendar** | 0.17.13 | 2026-07-28 | MIT/Apache-2.0 | hoodie/icalendar | — | 835,833 | iCalendar only | ❌ | ❌ | — | — |

Notes on the "ical-rs vcard" mentioned in the brief: the crates.io `vcard`
crate is **magiclen's** 4.0-only library, *not* ical-rs. ical-rs is archived
(last push 2024-08-17) and its vCard content-line work lives on in
`ical_vcard` (darkfire). `vobject` (rust-vobject) does have a `vcard.rs` but it
is a thin typed builder/reader and the README explicitly disclaims RFC
completeness and encodings.

### Per-crate detail

**vcard-rs (pimalaya) 0.4.0** — the only candidate that ships all three
capabilities the task asks for:
- Two layers: a byte-faithful CST (`VcardCst`) and a decoded model (`Vcard`),
  with `decode()` / `encode()` between them.
- Parser is maximally liberal and **round-trips byte-for-byte**, including
  blank lines, bare LF, RFC-style folds, QP soft breaks and unknown
  properties **[measured]**. Structural errors are real errors
  (`MissingCrlf`, `MissingPropertyColon`, `NonUtf8Header`, `ExpectedBegin`,
  `MissingEnd`).
- `Vcard::validate()` returns `Vec<VcardValidateError>` covering: property not
  defined in this version, disallowed value kind, disallowed parameter,
  cardinality, and closed value/parameter content (GENDER, PROFILE,
  CLIENTPIDMAP, PREF, PID, DERIVED). Passing mints a `VcardValid` proof.
- jCard read/write (RFC 7095) and JSContact conversion (RFC 9553/9555).
- Optional QP / base64 / foreign-charset decoding, each behind a feature.
- `no_std` core, no unsafe claims, light deps (24-node tree with defaults).
- Gaps for our use:
  - `decode()` **normalises an unknown/missing `VERSION` to 4.0**, and
    `validate()` then passes. `VERSION:5.0` parsed, round-tripped and
    validated OK in the throwaway test **[measured]**. The caller must read
    `VcardCst::version_line()` (raw) and reject anything not in
    {2.1, 3.0, 4.0}, and must reject 2.1 for CardDAV.
  - `UID` is optional in the RFC model, so CardDAV's "UID required" is not
    enforced; `FN` is `OneOrMore`, not "exactly one".
  - Group prefixes are **not stripped** in `decode()`: `item1.EMAIL` decodes to
    `Unknown("item1.EMAIL")` **[measured]**. Filter code must strip
    `prefix.` itself to match sabre.
  - No RFC 6868 parameter caret-decoding in the parser source (sabre does
    implement RFC 6868, `Parameter.php` L294).
  - No repair API (but `fill_required()` and in-place lens edits exist).
  - **Very new and low-adoption**: first release 2026-07-16, repo created
    2026-06-27, 1 star, 254 downloads, single maintainer (soywod/pimalaya,
    NLnet-funded). This is the main risk.

**calcard (stalwartlabs) 0.3.14** — production-proven (used by Stalwart
Mail Server, a real JMAP/CardDAV server), active (releases weekly), 75 stars,
52k downloads, Apache/MIT. Parses 2.1/3.0/4.0 and JSCalendar/JSContact, has
rkyv zero-copy archiving and a fuzz suite. But:
- It is deliberately **lenient** ("Postel's law"); `VCard::parse` returns the
  card even when lines are invalid, and the low-level `Parser` surfaces
  `Entry::InvalidLine` rather than erroring.
- It has **no validator** — no `validate()` that rejects missing FN/UID or bad
  cardinality.
- Serialization **normalises**: parameter case/order, `VERSION:5.0` → `4.0`,
  and it re-emits canonical text **[measured]**. Not byte-stable.
- Heavier dependency tree (chrono-tz, mail-builder, mail-parser, ahash).

**vcard4 / vcard (magiclen) / vcard_parser** — all **4.0-only**. `vcard4`
accepts a 3.0 card but silently rewrites it to 4.0 (including
`TYPE=INTERNET` → `TYPE=X-INTERNET`) **[measured]**; `vcard` rejects 2.1/3.0
outright **[measured]**. They cannot serve a CardDAV server that must accept
3.0 (and, for legacy import, 2.1) byte-stably.

**caldata (rustical)** — rustical, the main Rust CalDAV/CardDAV server, uses
`caldata` (fork/descendant of `ical`/`ical_vcard`), **not** `ical`/`icalendar`.
`rustical_ical::AddressObject::from_vcf()` parses with
`VcardParser::from_slice(...).expect_one()` and then **keeps the original
`String` bytes** (`vcf: OnceLock<String>`), re-emitting them for ETag/GET. So
rustical's model is "parse to sanity-check, store the raw bytes" — the same
architecture recommended here. `caldata` itself has no validator and no
byte-stable re-serialization.

**ical_vcard / vparser** — low-level content-line parsers, not vCard
semantics. `ical_vcard` is notable because it explicitly bounds
`max_line_length()` for untrusted input and implements RFC 6868. Useful as a
fallback foundation if a full library is rejected.

**jcard** — a standalone serde model of RFC 7095 jCard documents. Redundant if
vcard-rs is used (it has jCard built in).

---

## 3. Head-to-head on the four requirements

| Requirement | vcard-rs | calcard | vcard4 / vcard | caldata |
|---|---|---|---|---|
| (a) reject what CardDAV must reject | ✅ structural parse errors + `validate()` cardinality/param/value-kind; ❌ must add UID-required, 2.1-rejection, FN-exactly-once, version whitelist, control chars | ❌ no validator (only `InvalidLine` detection) | ❌ 4.0 only | ❌ no validator |
| (b) store byte-stable data | ✅ `to_bytes()` byte-exact | ❌ normalises | ❌ normalises | ❌ normalises |
| (c) never a security hole | ✅ no `unsafe` in 0.4.0 source (verified), proptest suite, light deps; ⚠️ no internal limits, young | ✅ production use, fuzz suite; ⚠️ no internal limits | ✅ small | ✅ small |
| (d) match sabre/vobject | ✅ closest available (rule-based cardinality/params, liberal parse); gaps: group stripping, version normalisation, no REPAIR, RFC 6868 | ⚠️ parse yes, validation/repair no | ❌ | ❌ |

---

## 4. RECOMMENDATION

### Primary: `vcard-rs` 0.4.x (lib name `vcard`), MIT OR Apache-2.0

Use it as the **parse + validate + byte-preserving edit** layer, with a thin
in-repo adapter module so it can be swapped:

```toml
vcard-rs = { version = "0.4", features = ["parser", "jcard", "quoted-printable", "base64", "encoding"] }
```

Why: it is the only maintained crate that simultaneously (1) parses 2.1/3.0/4.0
through one model, (2) round-trips bytes exactly, which is what lets the
sidecar keep stored blobs byte-stable and reproduce `readBlob`/`has_photo`
without a rewrite, (3) ships a rule-based validator that mirrors the shape of
sabre's `getValidationRules()`/`Property::validate()` (cardinality, allowed
params, allowed value kinds, version membership), and (4) does jCard natively.

Mitigate the maturity risk:
- Pin an exact version and vendor the crate (`cargo vendor`) or keep a local
  patch, since the API is pre-1.0.
- Wrap it behind an internal `trait VcardCodec` (parse, validate, version,
  properties, to_bytes, strip-photo) so the fallback can be dropped in
  without touching call sites.
- Treat the crate's parse/validate as one input to a **server-owned rule
  layer** (section 6); never rely on `validate()` alone.

### Fallback: `calcard` 0.3.x (Apache-2.0 OR MIT) + server-owned validation

If vcard-rs churns or a security review rejects a 0.4.0 dependency, use
`calcard` with `default-features = false` for parsing only, drive the
low-level `Parser` so `Entry::InvalidLine`/`UnexpectedComponentEnd`/
`UnterminatedComponent` are treated as rejections, read the raw `VERSION`
entry yourself, and store the original bytes. All of section 6 must then be
implemented by hand (calcard gives you nothing here). calcard is the
lower-risk dependency (production-proven in Stalwart, weekly releases), at
the cost of writing the whole validator.

### Do not use

`vcard4`, `vcard` (magiclen), `vcard_parser` (4.0 only), `vobject` (no
encodings, minimal), `ical`/`icalendar` (no vCard), `jcard` (redundant),
`ical-rs` (archived).

### Optional building block

`ical_vcard` 0.5 (MIT/Apache, no unsafe, RFC 6868, `max_line_length` bound) is
the best choice if you end up writing a content-line parser yourself; it is the
only candidate that exposes a built-in length limit for untrusted input.

---

## 5. Can any crate repair a vCard like `sabre/vobject`?

**No crate implements sabre's REPAIR semantics.** None has a `repair()`
function, a `REPAIR` flag, or level-1/2/3 messages.

- `vcard-rs` gets closest structurally: the forgiving CST keeps every byte, so
  the repairs sabre performs are all implementable as small, targeted edits
  (add a generated UID, derive FN from N/ORG/NICKNAME/EMAIL, `ENCODING=BASE64`
  → `B`, drop duplicate identical `?` properties, strip control chars,
  upper-case names). It even has `VcardCst::fill_required()` and in-place
  property lenses.
- `calcard`, `vcard4`, `vcard`, `caldata` have no repair primitives.

The honest option, and the one that matches sabre with the least surprise:

1. **Always keep the original bytes** and validate against them.
2. **Reject** (level-3 equivalent) everything that is not repairable or that
   sabre itself rejects.
3. Only for the specific, enumerated level-1 repairs, **build a repaired byte
   string** and store *that*; otherwise store the input verbatim. This is
   precisely sabre's "re-serialize only when there were messages, and only
   mark modified when the bytes changed" rule.
4. Do not silently re-serialize clean cards — that would break byte-stability
   and ETags.

"Accept the raw bytes and validate strictly" is the right default; repair is a
small opt-in layer over the same parsed tree, not a library feature you should
expect to buy.

---

## 6. Sanitization and limits to implement regardless of crate

vCard has no entity expansion, so classic **billion-laughs does not apply**
(that is XML/JSON). The real attack surface is linear resource blow-up and
stored-byte injection. None of the candidate crates imposes limits except
`ical_vcard` (`max_line_length`). **All of the following must be enforced by
the server.**

### Hard limits (reject with 4xx; never allocate unbounded)

1. **Body size.** Cap the request body before parsing. Match Nextcloud:
   `card_size_limit` default **5 242 880 bytes** (403 Forbidden). Enforce on
   the actual read bytes, not just `Content-Length` (chunked uploads).
2. **Logical line length after unfolding.** `ical_vcard` bounds this; nothing
   else does. Pick a bound (e.g. 256 KiB) so a single property folded across
   millions of physical lines cannot blow memory. **[measured: vcard-rs parses
   200 000 folded lines in 5.6 ms and allocates the whole logical value.]**
3. **Property count.** Cap (e.g. 10 000). **[measured: vcard-rs parses 500 000
   tiny properties in 54 ms with no complaint.]**
4. **Parameter count per property** and **parameter value length.** Cap
   (e.g. 64 params, 4 KiB each).
5. **Nesting / groups.** vCard groups are single-level `group.NAME`; there is
   no recursive nesting and no loop risk, but cap the number of groups and do
   not recurse on `.` — split once at the last `.`.
6. **Card count per request.** `parse_many` is a stream; for a single-resource
   `PUT`, reject more than one `BEGIN:VCARD`/`END:VCARD` pair.

### Content rules (match sabre)

7. **VERSION whitelist.** Exactly one `VERSION`, value in {2.1, 3.0, 4.0}.
   Reject anything else (vcard-rs would otherwise normalise to 4.0 and pass).
   Reject 2.1 under `PROFILE_CARDDAV` to match Nextcloud/sabre.
8. **UID required** for CardDAV, and **unique per address book** → 409
   `no-uid-conflict`.
9. **FN exactly once** (sabre) — vcard-rs only requires ≥1.
10. **UTF-8 + control characters.** Reject (or, under repair, transcode
    ISO-8859-1 and strip) `[\x00-\x08\x0B-\x0C\x0E-\x1F\x7F]`. Sabre flags
    these level 3; `StringUtil::isUTF8` explicitly rejects control chars.
    NUL is a control char and must not reach storage.
11. **Invalid UTF-8 values.** vcard-rs keeps values as raw bytes and
    round-trips them **[measured]**; the server must run an `ensureUTF8`
    equivalent (ISO-8859-1 → UTF-8, else reject/convert) and must not index a
    value it cannot decode.
12. **Property-name charset** `^[A-Z0-9-]+$` (upper-case; `_`→`-` under
    repair). vcard-rs only rejects non-UTF-8 names, not invalid characters.
13. **CRLF / stored-byte injection.** A vCard value cannot contain an
    unescaped newline by grammar, but a *decoded* value can contain `\n`
    (from `\n` escapes). If you ever re-serialize a decoded value, **re-escape
    it** (`\n`, `\r`, `\\`, `,`, `;`) or you inject a new property line into
    the stored blob. Prefer storing the original bytes and never re-encoding
    clean cards.
14. **ENCODING by version** (sabre): 4.0 forbids `ENCODING`; 2.1 allows
    `QUOTED-PRINTABLE|BASE64|8BIT`; 3.0 allows `B` (repair `BASE64`→`B`).
15. **PHOTO / binary.** Decode base64/QP only when needed; cap the decoded
    size. Keep Nextcloud's `readBlob` behaviour: drop `PHOTO:data:` payloads
    that are not `data:image/...` (the sidecar already ports this).
16. **Indexed-value truncation.** Match `mb_strcut(value, 0, 254)` on
    `INDEXED_PROPERTIES` (BDAY, UID, N, FN, TITLE, ROLE, NOTE, NICKNAME, ORG,
    CATEGORIES, EMAIL, TEL, IMPP, ADR, URL, GEO, CLOUD, X-SOCIALPROFILE) —
    truncate on a UTF-8 boundary, never mid-codepoint.
17. **jCard.** A body starting with `[` is jCard: parse as JSON, enforce the
    same size/count caps, re-serialize to vCard and mark modified (sabre does
    exactly this).
18. **No panics.** Fuzz the parse path; treat every `unwrap`/slice index on
    untrusted bytes as a bug. `vcard-rs` 0.4.0 contains no `unsafe` (verified
    by grepping the published source) and ships a proptest suite, but the
    server should still fuzz its own adapter.

Which crates impose any of these for you: **only `ical_vcard`** bounds line
length (`max_line_length()`). `vcard-rs`, `calcard`, `vcard4` and `vcard`
impose **no size/count limits**; size and count caps must be applied before
handing bytes to any of them.

---

## 7. What Nextcloud itself limits

- `card_size_limit` default **5 242 880 bytes**, checked in
  `CardDavValidatePlugin::beforePut()` (403).
- `mb_strcut($value, 0, 254)` for the indexed search table over
  `INDEXED_PROPERTIES`.
- UID uniqueness per address book (`getCardByUid` → 409 `no-uid-conflict`).
- `DAV\StringUtil::ensureUTF8()` on the card bytes before store/read
  (ISO-8859-1 → UTF-8 when the input is not valid UTF-8).
- `readBlob()` strips `PHOTO:data:` payloads that are not images.

---

## 8. Mapping to the read-only sidecar

For the current read-only sidecar, the library is used for three things, all
of which `vcard-rs` supports with the caveats above:

1. `addressbook-query` filter evaluation (`src/xml/filter.rs`): use the
   decoded model for names/params/values; **strip group prefixes yourself**
   (vcard-rs does not), and implement collations as today.
2. `has_photo` (`src/vcard.rs::has_photo`): read the `PHOTO` value; byte
   fidelity is not required for this predicate.
3. `filter_read_blob` (`src/vcard.rs::filter_read_blob`): keep the current
   CRLF-level implementation, or re-implement with the CST's in-place removal
   — the important property is that a card that needs no change comes back
   **byte-identical**, which `vcard-rs::to_bytes()` guarantees.

Because the sidecar is read-only, the parse path must treat stored bytes as
**untrusted** (legacy rows may contain anything) and must not panic or
allocate unboundedly; apply the caps from section 6 at read time too, not just
at write time.

---

## 9. Evidence URLs

Crates / docs:

- vcard-rs (pimalaya): https://crates.io/crates/vcard-rs · https://docs.rs/vcard-rs · https://github.com/pimalaya/vcard
  - Cargo.toml/features: https://github.com/pimalaya/vcard/blob/master/Cargo.toml
  - forgiving round-trip example: https://github.com/pimalaya/vcard/blob/master/examples/forgiving_parse.rs
  - validate example: https://github.com/pimalaya/vcard/blob/master/examples/validate_errors.rs
  - version normalisation to 4.0: https://github.com/pimalaya/vcard/blob/master/src/version.rs
  - validator scope: https://github.com/pimalaya/vcard/blob/master/src/validator.rs
  - parse error surface: https://github.com/pimalaya/vcard/blob/master/src/tree/error.rs
  - jCard: https://github.com/pimalaya/vcard/blob/master/examples/jcard.rs
- calcard (stalwartlabs): https://crates.io/crates/calcard · https://docs.rs/calcard · https://github.com/stalwartlabs/calcard
  - vCard parser: https://github.com/stalwartlabs/calcard/blob/main/src/vcard/parser.rs
- vcard4: https://crates.io/crates/vcard4 · https://docs.rs/vcard4 · https://github.com/tmpfs/vcard4
- vcard (magiclen): https://crates.io/crates/vcard · https://github.com/magiclen/vcard
- vcard_parser: https://crates.io/crates/vcard_parser · https://docs.rs/vcard_parser · https://github.com/kenianbei/vcard_parser
- caldata (rustical): https://crates.io/crates/caldata · https://github.com/lennart-k/caldata-rs
- rustical vCard handling: https://github.com/lennart-k/rustical/blob/main/crates/ical/src/address_object.rs · https://github.com/lennart-k/rustical/blob/main/Cargo.toml
- ical_vcard: https://crates.io/crates/ical_vcard · https://codeberg.org/darkfire/ical_vcard
- vparser: https://crates.io/crates/vparser · https://sr.ht/~whynothugo/vdirsyncer-rs
- jcard: https://crates.io/crates/jcard · https://docs.rs/jcard
- vobject: https://crates.io/crates/vobject · https://github.com/untitaker/rust-vobject
- ical (archived): https://crates.io/crates/ical · https://github.com/Peltoche/ical-rs
- icalendar: https://crates.io/crates/icalendar · https://github.com/hoodie/icalendar

sabre / Nextcloud (local checkout `nextcloud-server`):

- `3rdparty/sabre/dav/lib/CardDAV/Plugin.php::validateVCard()` (~L308)
- `3rdparty/sabre/vobject/lib/Component/VCard.php::validate()` (L221), `getValidationRules()` (L313)
- `3rdparty/sabre/vobject/lib/Component.php::validate()` (L571)
- `3rdparty/sabre/vobject/lib/Property.php::validate()` (L529)
- `3rdparty/sabre/vobject/lib/Property/Text.php::validate()` (L370)
- `3rdparty/sabre/vobject/lib/StringUtil.php::isUTF8()` / `convertToUTF8()`
- `3rdparty/sabre/vobject/lib/Parameter.php` (RFC 6868, ~L294)
- `3rdparty/sabre/dav/lib/DAV/StringUtil.php::ensureUTF8()` (L78)
- `3rdparty/sabre/dav/lib/CardDAV/Card.php` (L92), `AddressBook.php` (L142)
- `apps/dav/lib/CardDAV/CardDavBackend.php` (`getUID` L1571, `INDEXED_PROPERTIES` L47, `mb_strcut` L1448)
- `apps/dav/lib/CardDAV/Validation/CardDavValidatePlugin.php` (L34)

RFCs:

- RFC 6350 (vCard 4.0): https://www.rfc-editor.org/rfc/rfc6350
- RFC 2426 (vCard 3.0): https://www.rfc-editor.org/rfc/rfc2426
- vCard 2.1 (versit): https://www.imc.org/pdi/vcard-21.txt
- RFC 7095 (jCard): https://www.rfc-editor.org/rfc/rfc7095
- RFC 6868 (parameter value encoding): https://www.rfc-editor.org/rfc/rfc6868
- RFC 6352 (CardDAV): https://www.rfc-editor.org/rfc/rfc6352

---

## 10. Bottom line

- **Recommended:** `vcard-rs` 0.4.x (MIT OR Apache-2.0) for parse + strict
  validation + byte-exact storage, behind a thin adapter, pinned and vendored.
- **Fallback:** `calcard` 0.3.x (Apache-2.0 OR MIT) for parsing, with the whole
  validator written in-repo.
- **Non-negotiable:** the server's own rule layer and the limits in section 6;
  no crate imposes them, and none implements sabre's REPAIR.
- **Blocker to plan around:** `vcard-rs` is very young (0.4.0, 1 star, 254
  downloads) and normalises an unknown `VERSION` to 4.0 — the adapter must
  read the raw version line and reject 2.1/unknown versions itself.
