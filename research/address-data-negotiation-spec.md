# `{urn:ietf:params:xml:ns:carddav}address-data` negotiation & conditional GET — behavioural spec

Reverse-engineered from the exact production sources extracted from `nextcloud:33.0.5-apache`
(`target/sabre-ref/`). Every claim carries a `file:line` citation. Where behaviour cannot be
proven from these trees, the text says **UNPROVEN**.

Extracted tree layout (as found on disk — note the flattening):

| shorthand | path | what it is |
|---|---|---|
| *Plugin.php* | `target/sabre-ref/dav-lib/CardDAV/Plugin.php` | sabre/dav CardDAV plugin |
| *dav-lib* | `target/sabre-ref/dav-lib/` | `3rdparty/sabre/dav/lib` (`CardDAV/`, `DAV/`, … at top level) |
| *vobject-lib* | `target/sabre-ref/vobject-lib/` | `3rdparty/sabre/vobject/lib` (vobject **4.5.6**, `vobject-lib/Version.php:17`) |
| *http-lib* | `target/sabre-ref/http-lib/` | `sabre/http` 5.1.12 (`http-lib/Version.php:19`) — needed for Accept negotiation & date parsing |
| *nc-dav-lib* | `target/sabre-ref/nc-dav-lib/` | `apps/dav/lib` |

Nextcloud version stamp: `target/sabre-ref/version.php:2` (`$OC_VersionString = '33.0.5'`).

**UNPROVEN (structural):** `sabre/xml` (`Sabre\Xml\Element\Elements`, `KeyValue`, `Reader`) is
NOT in the extraction (no `xml-lib/` exists under `target/sabre-ref/`). Claims that depend on
XML *request deserialisation* mechanics outside the extracted `dav-lib/CardDAV/Xml/` classes are
marked accordingly.

---

## A. address-data generation

### A0. The code path

- The CardDAV plugin registers `propFind` handlers `propFindEarly`/`propFindLate` and the
  `report` handler at `target/sabre-ref/dav-lib/CardDAV/Plugin.php:66-68`.
- The **only** producer of the `address-data` property value is the `ICard` branch of
  `propFindEarly` (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:160-171`):

```php
$propFind->handle('{'.self::NS_CARDDAV.'}address-data', function () use ($node) {
    $val = $node->get();
    if (is_resource($val)) {
        $val = stream_get_contents($val);
    }

    return $val;
});
```

- Both REPORTs fetch the property through this same propFind machinery
  (`getPropertiesForMultiplePaths(...$report->properties)` at
  `target/sabre-ref/dav-lib/CardDAV/Plugin.php:241`; `getPropertiesForPath($href,
  $report->properties, 0)` at `target/sabre-ref/dav-lib/CardDAV/Plugin.php:453`) and then
  post-process the value with `convertVCard()`
  (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:243-246` multiget,
  `target/sabre-ref/dav-lib/CardDAV/Plugin.php:456-460` query).
- `ICard::get()` returns the stored blob verbatim: `Sabre\CardDAV\Card::get()` returns
  `$this->cardData['carddata']` (`target/sabre-ref/dav-lib/CardDAV/Card.php:67-76`); Nextcloud's
  `OCA\DAV\CardDAV\Card` does **not** override `get()`
  (`target/sabre-ref/nc-dav-lib/CardDAV/Card.php:11-42`) — the only mutation on read is the
  backend-level `readBlob()` filter (see A8).

### A1. Plain `<card:address-data/>` — verbatim or re-serialised?

**Answer: the stored `carddata` blob is returned VERBATIM (byte-for-byte), in PROPFIND always and
in the REPORTs whenever no version conversion and no prop-filter is in play. It is never
re-serialised in the plain case.** Two provisos:

1. In the REPORTs the blob is still **parsed** even in the verbatim branch (then returned
   unchanged), so an unparseable card is a hard error there (A6), while PROPFIND passes it
   through untouched.
2. Nextcloud's `readBlob()` may already have removed non-image `PHOTO:data:` lines from what
   `get()` returns (A8) — "verbatim" means "verbatim as served by `get()`".

The exact branch, `target/sabre-ref/dav-lib/CardDAV/Plugin.php:824-831` (inside `convertVCard`):

```php
case 'vcard3':
    if (VObject\Document::VCARD30 === $input->getDocumentType()) {
        // Do nothing
        return $data;
    }
    $output = $input->convert(VObject\Document::VCARD30);

    return $output->serialize();
```

Supporting facts:

- `convertVCard()` **always** parses first — `$input = VObject\Reader::read($data);` at
  `target/sabre-ref/dav-lib/CardDAV/Plugin.php:808` — and `$data` is returned unchanged in the
  branch above (no `$input->serialize()` happened unless a prop filter was applied,
  `target/sabre-ref/dav-lib/CardDAV/Plugin.php:809-818`). So "verbatim" is byte-identical to
  `get()`, not a re-serialisation.
- The same early-return exists for `vcard4` when the card is already 4.0
  (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:832-835`).
- PROPFIND never reaches `convertVCard()` at all: the handler above returns the raw string and
  nothing post-processes it (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:164-171`). The
  `version`/`content-type` attributes of the request element cannot influence this handler —
  PROPFIND collects property *names* only via `Sabre\Xml\Element\Elements`
  (`target/sabre-ref/dav-lib/Xml/Request/PropFind.php:58-78` maps `{DAV:}prop` to that class).
  **UNPROVEN:** the `Elements` implementation lives in unextracted `sabre/xml`; that attributes
  are dropped cannot be shown line-by-line here, but no attribute value is ever passed to the
  propFind handler anywhere in the extracted code.

### A2. The `version` attribute

**Parsing of the attribute** — `target/sabre-ref/dav-lib/CardDAV/Xml/Filter/AddressData.php:50-53`:

```php
$result = [
    'contentType' => $reader->getAttribute('content-type') ?: 'text/vcard',
    'version' => $reader->getAttribute('version') ?: '3.0',
];
```

Defaults: `content-type=text/vcard`, `version=3.0` (both attributes always end up non-empty in
the report object — `AddressBookMultiGetReport.php:99-101` /
`AddressBookQueryReport.php:144-145` union these keys in when `<card:address-data>` is present in
`<DAV:prop>`).

**How the value is used** — it is *not* interpreted as a version string. It is concatenated into
a synthetic media type that is then negotiated against a fixed option list:
`$contentType .= '; version='.$version;` (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:226-230`
multiget, `target/sabre-ref/dav-lib/CardDAV/Plugin.php:413-416` query), then
`negotiateVCard($contentType)` (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:756-789`), which
calls `Sabre\HTTP\negotiateContentType` over exactly these five options
(`target/sabre-ref/dav-lib/CardDAV/Plugin.php:758-772`): `text/x-vcard`, `text/vcard`,
`text/vcard; version=4.0`, `text/vcard; version=3.0`, `application/vcard+json`, mapping to
targets `vcard3` (default) / `vcard4` / `jcard`
(`target/sabre-ref/dav-lib/CardDAV/Plugin.php:777-787`).

Negotiation mechanics (`target/sabre-ref/http-lib/functions.php:104-184`,
`target/sabre-ref/http-lib/functions.php:312-356`):

- option parameters must all appear on the proposal with an exactly equal `name=value` string
  (`target/sabre-ref/http-lib/functions.php:150-159`; `parseMimeType` stores the whole `name=value`
  token as the parameter "value", `target/sabre-ref/http-lib/functions.php:351`);
- **case-sensitive** — nothing is lower-cased in `parseMimeType`
  (`target/sabre-ref/http-lib/functions.php:322-353`) and comparisons are `!==` / `array_key_exists`;
- winner = highest `q`, then highest specificity (20 type + 10 subtype + count of *option*
  parameters, `target/sabre-ref/http-lib/functions.php:164-167`), then lowest option index
  (`target/sabre-ref/http-lib/functions.php:170-179`); no match ⇒ `null`
  (`target/sabre-ref/http-lib/functions.php:183`) ⇒ `negotiateVCard`'s `default:` ⇒ `vcard3`
  (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:777-781`).

**Recognised `version` values** (with `content-type` at its default `text/vcard`):

| `version=` | proposal | matched option | target |
|---|---|---|---|
| *(absent/empty* ⇒ *default `3.0`)* | `text/vcard; version=3.0` | `text/vcard; version=3.0` (spec. 31 beats 30) | `vcard3` |
| `3.0` (exact, lowercase `version`) | idem | idem | `vcard3` |
| `4.0` | `text/vcard; version=4.0` | `text/vcard; version=4.0` | `vcard4` |
| `2.1` | `text/vcard; version=2.1` | only bare `text/vcard` (its option has no params) | **`vcard3`** |
| `4.00`, `4`, `junk`, or any case-variant (`4.0` is matched as the literal string `version=4.0`) | no param-matching option | `text/vcard` | `vcard3` |

So **2.1 is never a target** and unknown values silently degrade to "3.0". If
`content-type="text/x-vcard"` is used, only the `text/x-vcard` option can match (subtype
comparison at `target/sabre-ref/http-lib/functions.php:145-148`) ⇒ `vcard3` **regardless of
`version`**. Also note the code always appends `; version=…` (the attribute defaults to `3.0`),
so `content-type="text/vcard; version=4.0"` without a `version` attribute becomes
`text/vcard; version=4.0; version=3.0` and `parseMimeType` keeps the **last** `version` param
(`target/sabre-ref/http-lib/functions.php:343-351`) ⇒ `vcard3`. A syntactically invalid
`content-type` (no `/`) makes `parseMimeType` `var_dump()` and `exit`
(`target/sabre-ref/http-lib/functions.php:325-329`) — a hard, uncaught response abort.
**UNPROVEN:** the exact wire output of that `exit` path (it depends on PHP SAPI buffering).

**What each target does** (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:823-843`):

- `vcard3`: if the card's `getDocumentType()` is `VCARD30` → return data as-is (A1); else
  `convert(VCARD30)` + `serialize()`.
- `vcard4`: if already `VCARD40` → as-is; else `convert(VCARD40)` + `serialize()`.
- `jcard`: **always** `convert(VCARD40)` then `json_encode($output)` (A3).

`getDocumentType()` reads the `VERSION` property verbatim and matches only the exact strings
`2.1`, `3.0`, `4.0` (loose `switch`, string compare is case-sensitive), anything else ⇒
`Document::UNKNOWN`, uncached (`target/sabre-ref/vobject-lib/Component/VCard.php:146-166`).

**The converter**: `Sabre\VObject\Component\VCard::convert()`
(`target/sabre-ref/vobject-lib/Component/VCard.php:185-190`) →
`Sabre\VObject\VCardConverter::convert()`
(`target/sabre-ref/vobject-lib/VCardConverter.php:31-58`):

- identical in/out version ⇒ `clone` (`target/sabre-ref/vobject-lib/VCardConverter.php:34-35`)
  (unreachable through `convertVCard`, which short-circuits first);
- input may be 2.1/3.0/4.0, else `\InvalidArgumentException`
  (`target/sabre-ref/vobject-lib/VCardConverter.php:38-40`) — so a card whose `VERSION` is not
  one of the three exact strings produces `\InvalidArgumentException` → **HTTP 500** (C1/A6);
- target may only be 3.0 or 4.0 (`target/sabre-ref/vobject-lib/VCardConverter.php:41-43`);
- output skeleton: `new Component\VCard(['VERSION' => '4.0'|'3.0'])`
  (`target/sabre-ref/vobject-lib/VCardConverter.php:45-49`) which pulls in `getDefaults()`
  (B7), then the auto-generated default UID is removed
  (`target/sabre-ref/vobject-lib/VCardConverter.php:51-52`);
- input `VERSION` and `PRODID` are skipped (`target/sabre-ref/vobject-lib/VCardConverter.php:68-71`);
- every other property goes through `convertProperty()`
  (`target/sabre-ref/vobject-lib/VCardConverter.php:54-56, 66-242`).

#### COMPLETE rule list, 3.0 → 4.0 (target `VCARD40`)

Per-property (`target/sabre-ref/vobject-lib/VCardConverter.php:146-221`, common part 73-90, 223-241):

1. `NAME`, `MAILER`, `LABEL`, `CLASS` are **dropped** (148-150).
2. `VALUE=…` param is extracted and removed from the param list; effective value type =
   `VALUE` param value or the property's default (73-81). `PHONE-NUMBER` value type is coerced to
   the default for non-3.0 targets (82-84).
3. The property is recreated with `getParts()` as its multi-value (85-90) — so
   **NICKNAME/ORG/CATEGORIES/TEL/EMAIL multi-values pass through unchanged as part lists**;
   there is **no** multi-value merging/splitting and **no TEL/EMAIL TYPE defaulting** anywhere in
   this file (nothing inserts `TYPE=VOICE`/`TYPE=INTERNET` etc.).
4. Any `Property\Binary` property → `convertBinaryToUri()`
   (`target/sabre-ref/vobject-lib/VCardConverter.php:255-293`):
   value becomes `data:<mime>;base64,<base64-of-raw-bytes>` (290); mime sniffed from the `TYPE`
   param parts `JPEG`/`PNG`/`GIF` (case-insensitive) → `image/jpeg|png|gif`, those parts removed
   from `TYPE` (rest kept; `TYPE` dropped if emptied), else `application/octet-stream` (265-288).
5. Date-and-or-time properties with `X-APPLE-OMIT-YEAR`: if the value's year equals the param
   value the year is stripped to `--MM-DD`; the param is always removed (154-165).
6. `X-ABSHOWAS` with value `COMPANY` (case-insensitive) → `KIND:ORG` (168-171).
7. `X-ADDRESSBOOKSERVER-KIND` with value `GROUP` (case-insensitive) → `KIND:GROUP` (173-176).
8. `X-ADDRESSBOOKSERVER-MEMBER` → `MEMBER` (178-179).
9. `X-ANNIVERSARY` → `ANNIVERSARY`; silently dropped if an `ANNIVERSARY` with the same value
   already exists (181-189).
10. `X-ABDATE` (grouped, whose group's `X-ABLABEL` is exactly `_$!<Anniversary>!$_`) →
    `ANNIVERSARY` (same dedupe) (191-211). All other `X-ABDATE` kept as-is.
11. `X-ABLABEL` with value `_$!<Anniversary>!$_` → **dropped** (213-218).
12. `KIND`/`MEMBER`/everything else: copied as-is (no KIND special-casing on the 4.0 side beyond
    6-8).
13. Property group is preserved (223-224).
14. Parameters (`convertParameters40`, `target/sabre-ref/vobject-lib/VCardConverter.php:350-381`):
    - 2.1 nameless parameters get their guessed name (353-357);
    - `TYPE`: each part checked case-insensitively for `PREF` → becomes `PREF=1`; all other parts
      re-added as `TYPE` parts (359-369);
    - `ENCODING` and `CHARSET` **dropped** (371-374);
    - every other param copied with all its parts, order preserved (375-378).
15. A `VALUE=` param is (re)added iff the property's effective value type differs from the
    default for its name in the target document (232-239).

#### COMPLETE rule list, 4.0 → 3.0 (target `VCARD30`)

(`target/sabre-ref/vobject-lib/VCardConverter.php:92-145`, params 386-420)

1. `PHOTO`/`LOGO`/`SOUND` `Property\Uri` → `convertUriToBinary()`
   (`target/sabre-ref/vobject-lib/VCardConverter.php:306-345`): **only `data:` URIs** are
   converted (310-313); the payload is base64-decoded only if the media-type part contains `;`
   (i.e. `;base64`), otherwise the raw text after the comma is used (322-328); result is a
   `BINARY` property with `ENCODING=b` and `TYPE=JPEG|PNG|GIF` when the mime is
   `image/jpeg|png|gif` (331-342).
2. Date-and-or-time with year-less value (v4 `--MM-DD`): year replaced by `1604-…` and
   `X-APPLE-OMIT-YEAR=1604` added (95-108).
3. `ANNIVERSARY` → renamed `X-ANNIVERSARY` **and** a new Apple pair is appended: `ITEM<n>.X-ABDATE`
   (same value, `VALUE=DATE-AND-OR-TIME`) + `ITEM<n>.X-ABLABEL:_$!<Anniversary>!$_`, where `n` is
   the first `ITEM<n>` group not already present (110-124).
4. `KIND` (case-insensitive value): `org` → `X-ABSHOWAS:COMPANY` (126-131); `individual` →
   **dropped** (134-136); `group` → `X-ADDRESSBOOKSERVER-KIND:GROUP` (138-141); any other value
   keeps `KIND` unchanged (no `default` in the switch).
5. `MEMBER` → `X-ADDRESSBOOKSERVER-MEMBER` (143-145).
6. Parameters (`convertParameters30`, `target/sabre-ref/vobject-lib/VCardConverter.php:386-420`):
   - nameless 2.1 params named (390-393);
   - `ENCODING=QUOTED-PRINTABLE` dropped, any other `ENCODING` kept (395-401);
   - `PREF=1` → `TYPE=PREF`; any other `PREF` value **dropped** (404-412);
   - others copied verbatim with parts (413-417).
7. Same `VALUE=` re-add rule as above (232-239); group preserved (223-224).

**2.1 input** is accepted (`target/sabre-ref/vobject-lib/VCardConverter.php:38`) and follows the
3.0-target rules when converting to 3.0 (so "requesting 2.1" on a 2.1 card *converts it to 3.0*,
see below). No rule in this file touches `GEO`, `SORT-STRING`, `UID`, `CATEGORIES` value types or
`CONFIDENTIAL` (which is a *value* of `CLASS`, a property that rule 1 above drops for 4.0
targets). `URI`-typed and `TEXT`-typed values are re-emitted through their normal write path (B4).

**Requesting specific combinations:**

- **3.0 from a 4.0 card** → full 4.0→3.0 rule set above, then `serialize()`.
- **2.1** → never a target. `version=2.1` negotiates to `vcard3` (table above): a stored 3.0 card
  comes back **verbatim**; a stored 2.1 card is **converted to 3.0**; a stored 4.0 card is
  converted to 3.0 (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:824-831`).
- **Unknown value** → identical to `3.0` (same fallback).
- **Same version as stored** (`version=3.0` on 3.0, `version=4.0` on 4.0, `jcard` aside):
  **verbatim original bytes** (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:825-827, 833-835`) —
  unless a `<card:prop>` filter is present, in which case it is the parse→filter→`serialize()`
  output (A4).
- **`jcard`** always converts to 4.0 first (even a 4.0 card goes through `convert()`, which
  returns a clone at `target/sabre-ref/vobject-lib/VCardConverter.php:34-35`).

### A3. The `content-type` attribute

**Recognised values** are exactly the five negotiation options
(`target/sabre-ref/dav-lib/CardDAV/Plugin.php:758-772`): `text/x-vcard`, `text/vcard`,
`text/vcard; version=4.0`, `text/vcard; version=3.0`, `application/vcard+json` (matched as
described in A2 — case-sensitive, params must equal exactly). Anything else ⇒ no match ⇒
`default:` branch ⇒ target `vcard3`, mime `text/vcard`
(`target/sabre-ref/dav-lib/CardDAV/Plugin.php:777-781`). There is **no** 406; unknown content-type
is silently answered as vCard 3.0.

**`application/vcard+json`** ⇒ target `jcard`
(`target/sabre-ref/dav-lib/CardDAV/Plugin.php:786-787`) ⇒
`$input->convert(VCARD40)` then `json_encode($output)` with **default flags**
(`target/sabre-ref/dav-lib/CardDAV/Plugin.php:840-843`) — i.e. compact separators (no pretty
print), non-ASCII escaped as `\uXXXX` (no `JSON_UNESCAPED_UNICODE`), `/` escaped as `\/` (no
`JSON_UNESCAPED_SLASHES`). Because the document is force-converted to 4.0 first, a 3.0
`PHOTO;ENCODING=b` appears in jCard as a `data:` URI property (A2 rule 4).

Exact jCard shape:

- root: `VCard::jsonSerialize()` returns `["vcard", [ …properties… ]]` — **no** third
  "components" slot (`target/sabre-ref/vobject-lib/Component/VCard.php:454-467`).
- property (`target/sabre-ref/vobject-lib/Property.php:297-321`):
  `[ lcName, paramsObject, lcValueType, ...values ]`:
  - name lower-cased (297-299 via `strtolower($this->name)`, 315);
  - params object: keys lower-cased; the `VALUE` parameter is **omitted** (302-306); a property
    group is encoded as a `group` parameter (308-311);
  - parameter value = `Parameter::jsonSerialize()` = the raw stored value — a **string** for a
    single value, a **JSON array** for a multi-valued parameter (parser merges repeated/comma
    values into an array: `target/sabre-ref/vobject-lib/Parser/MimeDir.php:385-398`), `null` for a
    valueless parameter (`target/sabre-ref/vobject-lib/Parameter.php:322-329`). So multi-valued
    `TYPE=work,voice` → `"type": ["work","voice"]`; duplicated equal values are collapsed at parse
    (`target/sabre-ref/vobject-lib/Parser/MimeDir.php:393-395`).
  - value type: `strtolower(getValueType())` (315) — e.g. `text`, `uri`, `binary`,
    `date-and-or-time` (`target/sabre-ref/vobject-lib/Property/VCard/DateAndOrTime.php:39-42`),
    and **`unknown` for X- / unrecognised properties**
    (`target/sabre-ref/vobject-lib/Property/Unknown.php:37-39`).
  - values (`getJsonValue()`):
    - multi-value text (`NICKNAME`, `CATEGORIES`, …): each part a separate top-level array item;
      structured text (`N`, `ADR`, `ORG`, `GENDER`, `CLIENTPIDMAP` —
      `target/sabre-ref/vobject-lib/Property/Text.php:35-45`) wrapped as **one** item containing
      the parts array (`target/sabre-ref/vobject-lib/Property/Text.php:165-175`);
    - binary: single item = `base64_encode` of the raw bytes
      (`target/sabre-ref/vobject-lib/Property/Binary.php:94-97`);
    - unknown/X-: single item = the raw mime-dir (still escaped) value
      (`target/sabre-ref/vobject-lib/Property/Unknown.php:24-27`).

### A4. The child `<card:prop>` filter

**Parsing** (`target/sabre-ref/dav-lib/CardDAV/Xml/Filter/AddressData.php:55-62`): only child
elements `{urn:ietf:params:xml:ns:carddav}prop` **with a `name` attribute** are collected; the
attribute value is taken raw (case preserved); children without `name` are ignored; all matches
are collected (duplicates included).

**Semantics** (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:809-818`):

- Selection matches by **property name only** — parameters are not considered at all
  (`AddressData.php:60-62`, `Plugin.php:811-814`).
- **Case-sensitive**: candidate keys are the parsed children's `->name`, which the parser has
  upper-cased (`target/sabre-ref/vobject-lib/Parser/MimeDir.php:421`), and `array_diff()`
  (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:814`) is case-sensitive — so `name="EMAIL"`
  selects `EMAIL` but `name="email"` selects **nothing** (and `EMAIL` gets dropped).
- **Grouped properties match by base name**: `$child->name` excludes the group (group is a
  separate field, `target/sabre-ref/vobject-lib/Property.php:27-35`), so `item1.EMAIL` is selected
  by `name="EMAIL"` and *not* by `name="item1.EMAIL"` (which matches no key). The group survives
  filtering and is re-emitted (B3/B4).
- **X- properties and IANA tokens** are selectable exactly like standard ones (any name token
  parsed into `->name`; unrecognised names parse as `Property\Unknown`, see
  `target/sabre-ref/vobject-lib/Document.php:255-262`), again exact upper-case match.
- Properties **not** selected are removed — every occurrence, grouped or not:
  `unset($input->$key)` → `Component::__unset` → `remove($name)` which wipes the whole name-keyed
  child group (`target/sabre-ref/vobject-lib/Component.php:498-501, 139-143`).
- **Always retained regardless of the filter: `UID`, `VERSION`, `FN`** — merged into the list at
  `target/sabre-ref/dav-lib/CardDAV/Plugin.php:810` (`array_merge(['UID','VERSION','FN'], …)`).
  `PRODID` is **not** protected (it is dropped unless named).
- Listing the same property multiple times changes nothing (`array_diff` semantics).
- **ORDER of the output**: neither the request order nor strictly the original order. After
  filtering, `$input->serialize()` reorders: `VERSION` is hoisted to the first line
  (`target/sabre-ref/vobject-lib/Component.php:299-305`), all other properties keep the
  first-appearance order of their **name group** — children are stored keyed by name
  (`target/sabre-ref/vobject-lib/Component.php:119-124`) and `children()` concatenates per-name
  buckets (`target/sabre-ref/vobject-lib/Component.php:174-182`), so two `EMAIL`s separated by an
  `N` come out adjacent. Within one name, insertion order is preserved.
- **Filter alone (no version change) DOES trigger parse + re-serialise**: the filtered document is
  serialised at `target/sabre-ref/dav-lib/CardDAV/Plugin.php:818` and *that* string is what the
  same-version branch returns (825-827). So with a filter there is no verbatim path.
- **EMPTY `<card:prop/>`** → `addressDataProperties = []` (empty `array_map`,
  `target/sabre-ref/dav-lib/CardDAV/Xml/Filter/AddressData.php:60-62`) → the guard
  `if (!empty($propertiesFilter))` (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:809`) is false →
  **no filtering at all**; the full card is returned (verbatim for same-version). It does *not*
  mean "keep only UID/VERSION/FN".

### A5. Order of operations when combined

In `convertVCard` (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:803-855`), strictly:

1. parse (808);
2. **filter first** (809-818): name-based removal + `serialize()` into `$data`;
3. then, per target (823-843):
   - `vcard3`/`vcard4`: `convert()` (which copies only surviving properties) then
     `serialize()`, or return the step-2 `$data` when already the target version;
   - `jcard`: `convert(VCARD40)` (again: only surviving properties) then `json_encode`.

So conversion happens **after** filtering; VERSION/PRODID of the output are the converter's (B7),
and the JSON encodes the *converted, filtered* document. Note the converter runs on the filtered
document, so e.g. `X-ABDATE` anniversary detection (`VCardConverter.php:191-211`) only sees
surviving `X-ABLABEL`s — but `X-ABLABEL`/`X-ABDATE` are not in the default keep-list, so a filter
that omits them disables the anniversary synthesis.

### A6. Unparseable stored carddata

- **PROPFIND**: nothing parses the blob — raw passthrough inside the 207, no error
  (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:164-171`). Exception: if the client requests
  Nextcloud's `{http://nextcloud.com/ns}has-photo`, that handler parses
  (`target/sabre-ref/nc-dav-lib/CardDAV/HasPhotoPlugin.php:42-53`) and a broken card then faults.
- **REPORT (both)**: `convertVCard` parses unconditionally
  (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:808`) outside its `try` (the `try` at 820 covers
  only the switch), so `Sabre\VObject\ParseException`/`EofException` propagate. The query report
  can also fault earlier in `validateFilters` when filters are present
  (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:486`).
- Uncaught non-`Sabre\DAV\Exception` ⇒ `Server::start()` builds a `d:error` XML document with
  `s:exception` = `Sabre\VObject\ParseException` and `s:message`, and sets **HTTP 500**
  (`target/sabre-ref/dav-lib/DAV/Server.php:254-309`; generic `getHTTPCode()` = 500 at
  `target/sabre-ref/dav-lib/DAV/Exception.php:27-29`). **No raw passthrough** in the REPORT path.

### A7. PROPFIND vs addressbook-multiget vs addressbook-query

Common path: the value is always produced by the same propFind handler (A0). Differences:

| | PROPFIND on the card | `addressbook-multiget` | `addressbook-query` |
|---|---|---|---|
| value source | handler (`Plugin.php:164-171`) | same, via `getPropertiesForMultiplePaths` (`Plugin.php:241`) | same, via `getPropertiesForPath` (`Plugin.php:453`) |
| `version`/`content-type` attrs | no effect (A1) | honoured (`Plugin.php:226-234`) | honoured (`Plugin.php:413-420`) |
| `<card:prop>` child filter | n/a (attrs can't reach handler) | **parsed but IGNORED** — `convertVCard` is called with 2 args (`Plugin.php:243-246`), never passing `$report->addressDataProperties` (declared at `Xml/Request/AddressBookMultiGetReport.php:61`, filled at 85-101) | honoured (`Plugin.php:456-460`) |
| unparseable card | raw passthrough | 500 | 500 (earlier if `<filter>` present, `Plugin.php:486`) |
| requested property list | `<prop>` names of PROPFIND (only names — `Xml/Request/PropFind.php:58-78`) | `array_keys` of the report's `<DAV:prop>` (`Xml/Request/AddressBookMultiGetReport.php:97-98`) | idem (`Xml/Request/AddressBookQueryReport.php:142-143`) |

(The multiget's dropped filter is a real sabre behaviour: the same `addressDataProperties` key is
merged into the report object at `Xml/Request/AddressBookMultiGetReport.php:99-101` but the call
site at `Plugin.php:243-246` omits the third argument.)

### A8. What Nextcloud's `apps/dav` layer changes

- **`readBlob()` PHOTO stripping on every read** — `target/sabre-ref/nc-dav-lib/CardDAV/CardDavBackend.php:1048-1078`:
  splits the stored blob on `"\r\n"`; any line starting `PHOTO:data:` but **not**
  `PHOTO:data:image/` is removed together with its folded continuation lines (lines starting with
  a space). Applied in `getCardsFromQuery`/`getCard`/`getMultipleCards` (495, 535, 577) before the
  data is handed to `Card::get()`. Quirks: (a) if the blob *starts* with `PHOTO:data:` it is
  returned unfiltered (`1048-1057`, the "micro optimisation"); (b) grouped `item1.PHOTO:data:…`
  and bare-`\n` documents are not matched; (c) **the ETag is not recomputed** after stripping —
  only `size` is (`495-497`), so served bytes and stored ETag can disagree.
- **ETag shaping**: stored as bare `md5($cardData)`
  (`target/sabre-ref/nc-dav-lib/CardDAV/CardDavBackend.php:616, 689`) and wrapped `'"' . etag . '"'`
  on read (`492, 532, 574`) and on PUT return (`658, 719`) — a **strong** entity tag of the bytes
  as written (after `StringUtil::ensureUTF8` on PUT,
  `target/sabre-ref/dav-lib/CardDAV/Card.php:92`). Sabre's `Card::getETag()` md5 fallback is also
  strong-quoted (`target/sabre-ref/dav-lib/CardDAV/Card.php:124-137`).
- **uid column**: extracted at write time by parsing the card and reading `UID`
  (`getUID`, `target/sabre-ref/nc-dav-lib/CardDAV/CardDavBackend.php:1530-1543`); a create with a
  duplicate UID in the same addressbook is rejected with `BadRequest` (621-634). No uid rewriting
  of the card body occurs.
- **`OCA\DAV\CardDAV\Converter` is not a vCard version converter** — it *generates* system
  addressbook cards from user accounts
  (`target/sabre-ref/nc-dav-lib/CardDAV/Converter.php:73-199`); irrelevant to negotiation.
- **`propFindLate` Thunderbird quirk**: for User-Agents containing `Thunderbird`,
  `{DAV:}getcontenttype` is rewritten to `text/x-vcard` (charset stripped)
  (`target/sabre-ref/dav-lib/CardDAV/Plugin.php:671-688`).
- **`?export` on REPORT**: `MultiGetExportPlugin` re-reads the produced 207 XML and concatenates
  each response's `address-data` with `PHP_EOL` into one `text/vcard` download
  (`target/sabre-ref/nc-dav-lib/CardDAV/MultiGetExportPlugin.php:40-66`) — so the negotiated
  (converted/filtered) values are what gets exported.
- **`?photo` on GET**: `ImageExportPlugin` intercepts `method:GET` and serves the cached photo
  with `Cache-Control: private, max-age=3600, must-revalidate` and `Etag` (note lowercase header
  name) instead of the card body
  (`target/sabre-ref/nc-dav-lib/CardDAV/ImageExportPlugin.php:42-100`).
- `nc-dav-lib/CardDAV/Card.php` adds only identity/ACL helpers (11-42) — `get()` untouched.

---

## B. Re-serialisation fidelity (VObject MimeDir writer)

Note: the "MimeDir" writer is not a separate class — serialisation lives in
`Property::serialize()`/`Component::serialize()`; `vobject-lib/Parser/MimeDir.php` is the *parser*
(`Writer.php` just delegates, `target/sabre-ref/vobject-lib/Writer.php:24-27`).

### B1. Line folding

`target/sabre-ref/vobject-lib/Property.php:242-266`. The whole logical line (group + name +
`;params` + `:` + value, 244-253) is folded by:

```php
$str = \preg_replace(
    '/(
        (?:^.)?         # 1 additional byte in first line because of missing single space (see next line)
        .{1,74}         # max 75 bytes per line (1 byte is used for a single space added after every CRLF)
        (?![\x80-\xbf]) # prevent splitting multibyte characters
    )/x',
    "$1\r\n ",
    $str
);
```

- **Limit: 75 octets per physical line** — first line 75 content bytes, continuation lines =
  1 leading space + 74 content bytes (`Property.php:257-258`). Continuation prefix is a single
  `" "` (space) after CRLF (`Property.php:261`).
- **Bytes, not characters**: the regex has no `/u`, `.` matches a single byte (except `\n`);
  multibyte counting is per-byte. A UTF-8 character is never split: the `(?![\x80-\xbf])` guard
  backtracks the chunk so it never ends before a continuation byte (`Property.php:259`).
- **Fold points can be anywhere**: no respect for property-name/parameter boundaries, quoted
  parameter values, `name=value` pairs, or escape sequences — a fold may land between `\` and `n`
  of a `\n` escape or mid-base64. Only the multibyte-char guard constrains placement.
- The fold-inserted trailing `"\r\n "` of the last chunk has its final space chopped so each
  property string ends with `"\r\n"` (`Property.php:265-266`).

### B2. Line endings & document termination

- Every physical line ends `\r\n`: `BEGIN:<NAME>\r\n` (`target/sabre-ref/vobject-lib/Component.php:268`),
  each folded property (`Property.php:261-266`), and `END:<NAME>\r\n`
  (`target/sabre-ref/vobject-lib/Component.php:331`). The document therefore **always terminates
  with a trailing CRLF**.
- Parser side normalises input: `\r\n`/`\n` stripped per line (`rtrim($rawLine, "\r\n")`),
  empty lines skipped, continuations (leading SP or HTAB) unfolded by dropping exactly one leading
  whitespace char (`target/sabre-ref/vobject-lib/Parser/MimeDir.php:282-323`).

### B3. Parameter serialisation

`target/sabre-ref/vobject-lib/Parameter.php:264-311`, order from
`target/sabre-ref/vobject-lib/Property.php:158-169, 249-251`:

- **Order preserved** as first-insertion order of distinct parameter *names*; names are
  upper-cased at construction (`target/sabre-ref/vobject-lib/Parameter.php:60`) so case is
  normalised on re-serialise.
- Same-named parameters merge into one `NAME=v1,v2,…`: repeated params call `addValue()`
  (`target/sabre-ref/vobject-lib/Property.php:163-165`), and the parser merges repeated/comma
  values into an array (`target/sabre-ref/vobject-lib/Parser/MimeDir.php:385-398`) — exact
  duplicate values are collapsed (393-395). **`TYPE=` multi-values are emitted comma-joined in
  stored order** (`Parameter.php:276-303`).
- **Quoting**: a value is wrapped in `"` iff it contains any of `\n " : ; ^ , +`
  (`Parameter.php:299` regex — note `:` and `+` force quoting; `=` does not). Inside quotes
  RFC6868 escaping: `^`→`^^`, LF→`^n`, `"`→`^'` (`Parameter.php:300-308`). Unquoted values are
  emitted bare. A parameter with no value emits `NAME=` (`Parameter.php:268-270`).
- vCard 2.1 nameless parameters are re-emitted as bare tokens joined `;` when the document is 2.1
  (`Parameter.php:272-274`).
- Parser counterpart (`unescapeParam`, RFC6868 decode):
  `target/sabre-ref/vobject-lib/Parser/MimeDir.php:623-652`; quotes are stripped at
  `target/sabre-ref/vobject-lib/Parser/MimeDir.php:371-376`.

### B4. Value escaping on write

- **Text** (`target/sabre-ref/vobject-lib/Property/Text.php:125-155`): per sub-item `strtr` —
  `\`→`\\`, `;`→`\;`, `,`→`\,`, LF→`\n`, **CR → deleted** (`Text.php:140-150`). Sub-items of one
  component joined `,`, components joined by the delimiter: `;` for structured values (`N`, `ADR`,
  `ORG`, `GENDER`, `CLIENTPIDMAP` — `Text.php:35-45`, delimiter switch `Text.php:80-82`) and `,`
  for multi-value text. `N` is padded to 5 and `ADR` to 7 semicolon-components with empty strings
  (`Text.php:55-58, 129-131`).
- **FlatText** (`FN`, `TEL`, `EMAIL`, `GEO`, `SOUND`, `TITLE`, `ROLE`, `NOTE`, `UID`, `VERSION`,
  `KEY`, `LABEL`, `MAILER`, `SORT-STRING`, `PRODID`, `CLASS`, `KIND`, … per
  `target/sabre-ref/vobject-lib/Component/VCard.php:66-118`) inherits Text's write path with
  delimiter `,` (`target/sabre-ref/vobject-lib/Property/FlatText.php:25-32`) — so an *unescaped*
  `;` inside `FN`/`GEO`/`SOUND` is re-emitted **escaped** (`\;`), and an unescaped `,` splits
  into parts and is re-emitted as a delimiter (escaped only when it came in escaped).
- **URI** (`URL`, `PHOTO`/`LOGO` in v4 docs, `CALURI`, …): only `,`→`\,` on write and `\,`→`,`
  (plus URL-only `\:`→`:`) on read (`target/sabre-ref/vobject-lib/Property/Uri.php:71-114`); for
  `URL`/`PHOTO` a synthetic `VALUE=URI` parameter is appended at serialisation if absent
  (`target/sabre-ref/vobject-lib/Property/Uri.php:45-61`).
- **Binary**: raw bytes ↔ base64 (`target/sabre-ref/vobject-lib/Property/Binary.php:55-71`);
  the written value is one long `base64_encode` line that the generic fold (B1) then splits.
- **`DateAndOrTime`/dates** (`BDAY`, `ANNIVERSARY`, …) round-trip the raw value string through
  `Property` storage (no escape table applies to `getRawMimeDirValue` there); the converter
  rewrites values only per A2 rules.
- Newlines in text are encoded as the two characters `\n` (both `\N` and `\n` decode to LF on
  parse — `target/sabre-ref/vobject-lib/Parser/MimeDir.php:548-589`), i.e. re-serialised output
  always uses lowercase `\n`.
- vCard 2.1 documents take a different Text write path: parts joined `;` with only `;` escaped,
  and values containing LF are re-encoded `ENCODING=QUOTED-PRINTABLE` with hand-rolled QP and
  75-byte soft breaks (`target/sabre-ref/vobject-lib/Property/Text.php:195-235`).

### B5. ENCODING=QUOTED-PRINTABLE / BASE64 round trips

- **QUOTED-PRINTABLE is decoded at parse** and never re-encoded for 3.0/4.0 documents:
  detection and decode at `target/sabre-ref/vobject-lib/Parser/MimeDir.php:462-463`
  (`setQuotedPrintableValue`, `target/sabre-ref/vobject-lib/Property/Text.php:105-118`, FlatText
  variant `target/sabre-ref/vobject-lib/Property/FlatText.php:41-45`). The `ENCODING` parameter
  itself is *kept* in the parameter list (it is passed through `createProperty`, MimeDir 457-460),
  and the non-2.1 writer emits all parameters unfiltered
  (`target/sabre-ref/vobject-lib/Property/Text.php:198-199` → `Property.php:249-251`). Net effect
  for a 3.0 document: **QP is decoded and re-emitted as plain UTF-8 text, but with a stale
  `;ENCODING=QUOTED-PRINTABLE` still attached.** In 2.1 documents the writer drops the
  `QUOTED-PRINTABLE` param and re-encodes QP only when the value contains LF
  (`target/sabre-ref/vobject-lib/Property/Text.php:223-235`). On *conversion* to 3.0/4.0 the
  param is dropped (`target/sabre-ref/vobject-lib/VCardConverter.php:396-401` resp. 371-374).
- **BASE64/BINARY** (`ENCODING=b|B|BASE64`): the parser base64-decodes the (unfolded) value into
  raw bytes (`target/sabre-ref/vobject-lib/Property/Binary.php:55-62`; decode errors are silent),
  the writer re-emits `base64_encode` output re-folded at 75 bytes
  (`target/sabre-ref/vobject-lib/Property/Binary.php:69-71` + B1), with the `ENCODING` parameter
  kept verbatim (value case preserved, name upper-cased). So yes: **re-emitted as base64 with
  fresh line folding**; the base64 text itself is canonicalised (padding/whitespace normalised).
  In vCard 4 documents binary-typed properties are parsed as `Uri` instead
  (`target/sabre-ref/vobject-lib/Component/VCard.php:528-541`) and keep their `data:` URI as text.

### B6. Observable round-trip differences (what tests must assert)

Comparing stored bytes vs parse→`serialize()` output (no version conversion):

1. **Property and group names upper-cased** (`target/sabre-ref/vobject-lib/Parser/MimeDir.php:421`;
   group split at `target/sabre-ref/vobject-lib/Document.php:200-205`); parameter names upper-cased
   (`target/sabre-ref/vobject-lib/Parameter.php:60`); parameter value case preserved.
2. **Reordering**: `VERSION` moves to the first line
   (`target/sabre-ref/vobject-lib/Component.php:299-305`); same-name properties are coalesced at
   the first occurrence's position (`Component.php:119-124, 174-182`) — e.g. `FN,EMAIL,N,EMAIL`
   → `FN,EMAIL,EMAIL,N`. Everything else keeps relative order.
3. **Fold points recomputed** everywhere (75-octet, space continuation, may split escapes) — B1.
4. **Escape normalisation**: `\N` → `\n`; unescaped `;`/`,` in text values become escaped; CR
   bytes are **deleted** from text values; lone `\` becomes `\\`
   (`target/sabre-ref/vobject-lib/Property/Text.php:140-150`).
5. **Trailing whitespace in values is PRESERVED** — the parser only trims CR/LF
   (`target/sabre-ref/vobject-lib/Parser/MimeDir.php:298`); there is no value trimming anywhere on
   the write path. (The "trailing whitespace removed" hypothesis is **false** for these sources.)
6. **`N` padded to 5 / `ADR` padded to 7** semicolon-components (`Text.php:55-58, 129-131`).
7. **Base64 canonicalised** (decode→encode) and re-folded; **QP decoded** while its
   `ENCODING=QUOTED-PRINTABLE` param survives in 3.0+ documents (B5).
8. **Duplicate identical parameter values collapsed**; repeated same-name params merged to
   `NAME=a,b` (`target/sabre-ref/vobject-lib/Parser/MimeDir.php:385-398`,
   `target/sabre-ref/vobject-lib/Property.php:163-165`).
9. **Document hygiene**: UTF-8 BOM stripped (`target/sabre-ref/vobject-lib/Parser/MimeDir.php:151-156`),
   blank lines dropped (`MimeDir.php:293-299`), bare-`\n` input normalised to `\r\n` output,
   tab-folded continuations rewritten as space folds (`MimeDir.php:311-315` vs `Property.php:261`).
10. **No Unicode normalisation** (no NFC/NFD step anywhere in the write path); charset transcoding
    happens only on parse for vCard 2.1 `CHARSET=` (`target/sabre-ref/vobject-lib/Parser/MimeDir.php:465-480`)
    and on PUT via `StringUtil::ensureUTF8` (`target/sabre-ref/dav-lib/CardDAV/Card.php:92`).
11. **Guaranteed trailing `CRLF`** after `END:VCARD` (B2).
12. A `URL`/`PHOTO` (v4) property may **gain** a synthetic `VALUE=URI` parameter
    (`target/sabre-ref/vobject-lib/Property/Uri.php:45-61`).

### B7. VERSION / PRODID on convert

- **VERSION is rewritten**: the output document is constructed with `['VERSION' => '4.0'|'3.0']`
  (`target/sabre-ref/vobject-lib/VCardConverter.php:45-49`) and the input's `VERSION` is skipped
  (68-71) — so `VERSION:3.0` becomes `VERSION:4.0` on a 4.0 conversion and vice versa. On
  re-serialise the sort also forces VERSION first (`Component.php:299-305`).
- **PRODID**: input `PRODID` is dropped (68-71) and the output gets a fresh one from
  `VCard::getDefaults()`: `PRODID:-//Sabre//Sabre VObject 4.5.6//EN`
  (`target/sabre-ref/vobject-lib/Component/VCard.php:438-445`; version string
  `target/sabre-ref/vobject-lib/Version.php:17`). It is *added*, not kept.
- **UID**: the default generated UID of a fresh `VCard` is removed
  (`target/sabre-ref/vobject-lib/VCardConverter.php:51-52`), but the *input's* `UID` is copied by
  `convertProperty` (it is not in the skip list, 68-71) — so a converted card keeps its UID and
  gains `PRODID`. When **not** converting (verbatim or filter-only path), `VERSION`/`PRODID` are
  untouched (except `VERSION`'s position when a filter triggered re-serialisation).

---

## C. Conditional GET on a card (`GET`/`HEAD /addressbooks/…/card.vcf`)

### C1. `Server::checkPreconditions` — exact algorithm

`target/sabre-ref/dav-lib/DAV/Server.php:1289-1430`, invoked for **every** HTTP method before
dispatch (`target/sabre-ref/dav-lib/DAV/Server.php:466-469`). Evaluation order (each block runs
only if the earlier ones haven't returned/thrown):

1. **`If-Match`** (`target/sabre-ref/dav-lib/DAV/Server.php:1296-1335`):
   - resource missing ⇒ 412 (`1302-1304`);
   - `If-Match: *` ⇒ pass iff the resource exists (`1306-1310`);
   - else comma-separated list, each item `trim(..., ' ')` and compared to the node's ETag by
     **exact string identity** `$etag === $ifMatchItem` (`1313-1318`) — **no weak/strong
     comparison, no `W/` prefix stripping** (`W/"x"` never matches `"x"`); a legacy Evolution
     workaround retries after `str_replace('\\"', '"', …)` (`1320-1324`);
   - no match ⇒ **412** `PreconditionFailed` with `ETag` header set when available (`1328-1332`);
2. **`If-None-Match`** (`target/sabre-ref/dav-lib/DAV/Server.php:1336-1380`):
   - missing resource ⇒ condition ignored (`1342-1348`);
   - `*` ⇒ matches whenever the resource exists (`1353-1355`); else comma list, `trim` + the same
     **exact string identity** comparison (`1358-1365`) (also weak-unsafe: `W/"x"` in the header
     will not match a strong `"x"` and vice versa);
   - on match: set `ETag` header (when `$etag` non-null — note for `*` no ETag is computed here)
     and then **`GET` ⇒ 304** (`1368-1374`) vs **every other method (including `HEAD`) ⇒ 412**
     (`1375-1378`);
3. **`If-Modified-Since`** — only when **no** `If-None-Match` header was present
   (`target/sabre-ref/dav-lib/DAV/Server.php:1383`):
   - date parsed with `HTTP\parseDate` (`target/sabre-ref/http-lib/functions.php:29-68`), which
     accepts IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`), obsolete RFC 850
     (`Sunday, 06-Nov-94 08:49:37 GMT`) and ANSI C asctime (`Sun Nov 6 08:49:37 1994`, ` GMT`
     appended, `functions.php:59-62`); any other format ⇒ `false` ⇒ the condition is **silently
     ignored** (`target/sabre-ref/dav-lib/DAV/Server.php:1393`);
   - `lastMod <= date` ⇒ **304** + `Last-Modified` header, for **all** methods
     (`target/sabre-ref/dav-lib/DAV/Server.php:1396-1404`);
4. **`If-Unmodified-Since`** (`target/sabre-ref/dav-lib/DAV/Server.php:1409-1428`): same date
   parsing; `lastMod > date` ⇒ **412** (`1417-1424`); unparseable date ⇒ ignored;
5. `If:` state tokens (beyond this spec's scope; failure ⇒ 412 at
   `target/sabre-ref/dav-lib/DAV/Server.php:1483`).

**304 vs 412 mapping summary**: `If-Match` failure ⇒ 412; `If-None-Match` match ⇒ **304 for GET,
412 for everything else (HEAD included)**; `If-Modified-Since` fresh ⇒ 304 for all methods;
`If-Unmodified-Since` stale ⇒ 412. 412 is `Sabre\DAV\Exception\PreconditionFailed`
(`getHTTPCode()` = 412, `target/sabre-ref/dav-lib/DAV/Exception/PreconditionFailed.php:49-51`) and
surfaces as an XML error body via `Server::start()`
(`target/sabre-ref/dav-lib/DAV/Server.php:254-309`). 304 is produced by `$response->setStatus(304)`
+ `return false`, which makes `invokeMethod` send the response and stop before method dispatch
(`target/sabre-ref/dav-lib/DAV/Server.php:466-469`).

### C2. What PHP emits for the 304 on a card GET

Traced: `invokeMethod` → `checkPreconditions` returns false → `$this->sapi->sendResponse($response)`
(`target/sabre-ref/dav-lib/DAV/Server.php:466-469`). `CorePlugin::httpGet`
(`target/sabre-ref/dav-lib/DAV/CorePlugin.php:73-202`) and CardDAV's `httpAfterGet`
(`target/sabre-ref/dav-lib/CardDAV/Plugin.php:722-738`) are **not reached** (also no
`afterMethod:GET`, `target/sabre-ref/dav-lib/DAV/Server.php:476-478`).

Headers actually set by sabre on the 304 response:

- **`If-None-Match` path**: `ETag: <strong etag>` — only if `$etag` is non-null, i.e. the node is
  an `IFile` and the comparison loop ran; **bare `If-None-Match: *` sets no ETag**
  (`target/sabre-ref/dav-lib/DAV/Server.php:1353-1367`).
- **`If-Modified-Since` path**: `Last-Modified: <IMF-fixdate>` only
  (`target/sabre-ref/dav-lib/DAV/Server.php:1400-1402`, date format
  `target/sabre-ref/http-lib/functions.php:74-82`).
- **No** `Content-Type`, **no** `Content-Length`, **no** `Cache-Control` are set by any code on
  this path; **body is empty** — `Sapi::sendResponse` emits the status line + headers and, the body
  being `null`, returns without writing anything
  (`target/sabre-ref/http-lib/Sapi.php:62-88`).

**There is no `NotModified` exception class in this tree** (`dav-lib/DAV/Exception/` contains only
`PreconditionFailed` etc.; no `NotModified` symbol anywhere in the extracted PHP) — the 304 is
produced exclusively by `checkPreconditions` as above. (The task's "NotModified exception
handling" does not exist in these sources.)

For contrast — a **200** card GET: headers come from `getHTTPHeaders()`
(`target/sabre-ref/dav-lib/DAV/Server.php:849-875`, mapping `{DAV:}getcontenttype|getcontentlength|
getlastmodified|getetag`): `Content-Type: text/vcard; charset=utf-8`
(`target/sabre-ref/dav-lib/CardDAV/Card.php:114-117`), `Content-Length`, `Last-Modified`, strong
`ETag: "<md5>"`; body = `get()` verbatim (`CorePlugin.php:84-95, 193-198`). Then `afterMethod:GET`
runs `httpAfterGet`, which — if the response `Content-Type` contains `text/vcard` — negotiates the
request's `Accept` (A2 mechanics) and may replace the body (jCard or converted vCard), updating
`Content-Type` and `Content-Length` but **not** `ETag`/`Last-Modified`
(`target/sabre-ref/dav-lib/CardDAV/Plugin.php:722-738`).

**UNPROVEN (outside these trees):** any `Cache-Control` / `Content-Security-Policy` / `X-` headers
Nextcloud's front controller, PHP's SAPI or Apache might add to either the 304 or the 200 (e.g.
`mod_headers` rules); only sabre-level emission is proven here.

### C3. Weak-ETag-on-GET deviation

**Not present in these sources.** Exhaustive search of `nc-dav-lib` and `dav-lib` finds no `W/`
entity-tag construction or ETag-strength rewriting (the only `W/` hits are base64 noise inside
`nc-dav-lib/ExampleContentFiles/exampleContact.vcf`). All ETag *generation* in the extracted code
is strong: `'"' . md5($cardData) . '"'`
(`target/sabre-ref/nc-dav-lib/CardDAV/CardDavBackend.php:616, 658, 689, 719, 492, 532, 574`) and
`'"'.md5($data).'"'` (`target/sabre-ref/dav-lib/CardDAV/Card.php:124-137`), and `checkPreconditions`
passes the node's ETag through untouched (`Server.php:1366-1367`). If production PHP answers GET
with `W/"…"` while PROPFIND shows `"…"`, that transformation happens **outside** these three
libraries (Nextcloud core / web-server config): **UNPROVEN here**.

### C4. HEAD

- Preconditions run first with the literal method `HEAD`
  (`target/sabre-ref/dav-lib/DAV/Server.php:466`). Because the 304 branch of `If-None-Match` tests
  `'GET' === $request->getMethod()` (`target/sabre-ref/dav-lib/DAV/Server.php:1368-1378`), a
  **HEAD with a matching `If-None-Match` gets 412, not 304** (deviation from RFC 7232's "HEAD
  follows GET semantics" — derived directly from the cited branch; no HEAD special-casing exists
  before it).
- `If-Modified-Since` on HEAD **does** produce 304 (method-agnostic,
  `target/sabre-ref/dav-lib/DAV/Server.php:1396-1404`), with `Last-Modified` only.
- When preconditions pass, `CorePlugin::httpHead` re-enters `invokeMethod` with a cloned GET
  request flagged `X-Sabre-Original-Method: HEAD`
  (`target/sabre-ref/dav-lib/DAV/CorePlugin.php:242-250`); `httpGet` then emits the full header
  set (`Content-Type`, `ETag`, `Last-Modified`, `Content-Length`) but an **empty body**
  (body suppression at `target/sabre-ref/dav-lib/DAV/CorePlugin.php:82-84`; status 200 +
  `Content-Length` at `target/sabre-ref/dav-lib/DAV/CorePlugin.php:193-198`). The inner pass re-runs
  `checkPreconditions` as `GET` (so it *would* 304 an `If-None-Match`), but the outer pass already
  raised 412 in that case. Collection HEAD turning 501→200 with `X-Sabre-Real-Status`:
  `target/sabre-ref/dav-lib/DAV/CorePlugin.php:252-261`.
- **UNPROVEN:** whether Apache/PHP suppresses `Content-Length` on the 304/HEAD wire responses.

---

## UNPROVEN index

1. PROPFIND request attribute handling for `<card:address-data …>` (sabre/xml `Elements` class not
   extracted) — A1.
2. Wire output of the `parseMimeType` fatal `var_dump(); exit;` on a malformed `content-type`
   attribute — A2 (`http-lib/functions.php:325-329` shows the call, not the SAPI result).
3. Any weak-etag transformation on GET (C3) — must live outside `dav-lib`/`nc-dav-lib`/`vobject-lib`.
4. Headers added by PHP/Apache/Nextcloud front controller to 200/304/HEAD responses (C2/C4).
5. Exact `DateAndOrTime`/`TimeStamp` raw-value write quirks beyond "stored raw, re-emitted via
   `Property::serialize`" (B4) — the classes carry value validation (`setDateTime`) but no
   mime-dir escape table; edge formats were not line-cited exhaustively.
---

## Live parity verification (2026-09-26, `tests/local` harness: Nextcloud 33.0.5 + PHP's own vobject 4.5.6)

Two torture-test cards (v3 with structured `N`, `TYPE` multi-params, `PHOTO;ENCODING=b`,
Apple `X-ABDATE`/`X-ABLABEL`, escaped `FN`, `NOTE` with `\n`; v4 with year-less `BDAY`/
`ANNIVERSARY` and `KIND:group`) were written through the sidecar's native `PUT`, then every
negotiation path was requested from the sidecar (:17870) and from PHP (:18081) and the
`<card:address-data>` payloads compared byte-for-byte:

| case | result |
|---|---|
| plain `<card:address-data/>` (multiget) | **identical** |
| `version="4.0"` on the v3 card (3→4: `PHOTO;VALUE=URI:data:…\,…`, `KIND:ORG`, Apple pair) | **identical** |
| `version="3.0"` on the v4 card (4→3: `BDAY;X-APPLE-OMIT-YEAR=1604:…`, `ITEM1.X-ABDATE;VALUE=DATE-AND-OR-TIME:…`) | **identical** |
| `version="2.1"` (negotiates to the 3.0 target) | **identical** |
| `content-type="application/vcard+json"` (full jCard document) | **identical** |
| query report, `<card:prop name="EMAIL"/>` | **identical** |
| query report, `<card:prop name="email"/>` | differs — the **declared** case-insensitivity divergence (PHP drops `EMAIL`, the sidecar keeps it) |
| multiget with a `<card:prop>` filter | differs — the **declared** divergence (PHP's multiget call site omits the filter; the sidecar applies it) |

Conditional `GET`/`HEAD` status codes (sidecar vs live PHP):

| request | sidecar | PHP |
|---|---|---|
| `GET` + matching `If-None-Match` | 304 | 304 |
| `HEAD` + matching `If-None-Match` | 304 (RFC 7232) | **412** — the `'GET' === $method` quirk confirmed live |
| `GET` + `W/"…"` (weak form of the current tag) | 304 (weak comparison) | 200 — the declared divergence |
| malformed `If-Modified-Since` (`+0200` zone) | 200 | 200 — identical rejection |
