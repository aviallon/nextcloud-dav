# Recon: shared / group address books for the `nextcloud-dav` sidecar

Source of truth: `nextcloud-server` (Nextcloud 36-dev checkout at
`/home/aviallon/Programing/Opensource/nextcloud/nextcloud-server`). All
`apps/...`, `lib/...`, `3rdparty/...` paths below are relative to that repo;
`src/...` and `tests/...` paths are relative to `nextcloud-dav`.

Status: **design only** — nothing in this document is implemented. It describes
what the sidecar must do to serve `oc_dav_shares` books (`<uri>_shared_by_<owner>`)
with the same wire properties and the same write ACLs as PHP, while the
address-book *home* listing, the system book and the `contactsinteraction` book
stay on PHP for now.

---

## 0. Terminology / the `oc_dav_shares` row

`oc_dav_shares` (schema `apps/dav/lib/Migration/Version1004Date20170825134824.php:386-420`):

| column | meaning |
|---|---|
| `id` | autoincrement |
| `principaluri` | the **sharee** principal: `principals/users/<uid>` or `principals/groups/<gid>` (or `principals/circles/...`, `principals/remote-users/...`) |
| `type` | `'addressbook'` for CardDAV (`apps/dav/lib/CardDAV/Sharing/Service.php:15`) |
| `access` | `1` owner, `2` read-write, `3` read-only, `5` unshared tombstone (`apps/dav/lib/DAV/Sharing/Backend.php:24-29`) |
| `resourceid` | `oc_addressbooks.id` |
| `publicuri`, `token` | unused for address books |

Unique index `dav_shares_index (principaluri, resourceid, type, publicuri)`;
`SharingService::shareWith()` deletes then re-inserts
(`apps/dav/lib/DAV/Sharing/SharingService.php:22-26`), so a `(principaluri,
resourceid, type)` pair is normally unique even though PostgreSQL treats NULL
`publicuri` as distinct.

**Unshare is a tombstone, not a delete** for inherited (group/circle) access:
`Backend::unshare()` (`apps/dav/lib/DAV/Sharing/Backend.php:174-198`) deletes the
direct share, and if the sharee still has access through a group/circle it
inserts a new row with `access = 5` and the *sharee's* principal. A direct
unshare leaves only the tombstone.

---

## 1. Listing — which books a user sees, and the SQL

### 1.1 The rule

A user sees:

1. every `oc_addressbooks` row with `principaluri = 'principals/users/<uid>'`
   (owned books), plus
2. every `oc_addressbooks` row that has an `oc_dav_shares` row of
   `type = 'addressbook'` whose `principaluri` is one of:
   * the user's own principal `principals/users/<uid>`, **or**
   * a group principal `principals/groups/<urlencoded gid>` for each group the
     user is a member of (`Principal::getGroupMembership()`,
     `apps/dav/lib/Connector/Sabre/Principal.php:187-217`; groups for which
     `hideFromCollaboration()` is true are skipped, `:204-206`),

   **minus** any resource for which one of those principals has an unshared
   tombstone (`access = 5`).

Owned books are added first; shared rows are then folded in by `oc_addressbooks.id`
(`apps/dav/lib/CardDAV/CardDavBackend.php:105-190`).

### 1.2 The exact PHP query (`CardDavBackend::getAddressBooksForUser`)

Owned books (`:110-116`):

```sql
SELECT id, uri, displayname, principaluri, description, synctoken
FROM oc_addressbooks
WHERE principaluri = ?
```

Shared books (`:133-147`), literal current PHP:

```sql
SELECT a.id, a.uri, a.displayname, a.principaluri, a.description, a.synctoken, s.access
FROM oc_dav_shares s
JOIN oc_addressbooks a ON s.resourceid = a.id
WHERE s.principaluri IN (?, ?, ...)                 -- group principals + own principal
  AND s.type = 'addressbook'
  AND s.id NOT IN (
      SELECT d.id FROM oc_dav_shares d
      WHERE d.access = 5 AND d.principaluri IN (?, ?, ...)
  )
```

> **Upstream asymmetry — read this before copying the query.** The CalDAV twin
> of the same commit selects `resourceid` and excludes `a.id`
> (`apps/dav/lib/CalDAV/CalDavBackend.php:396-406`,
> `:1375-1387`), while CardDAV selects `id` and excludes `s.id`
> (`apps/dav/lib/CardDAV/CardDavBackend.php:140-147`). The `s.id` form only
> removes the *tombstone row itself*; it does **not** hide a resource the sharee
> still reaches through a group. Consequence: in current PHP, a user who
> unshares a group-shared address book still sees it (the group row survives).
> The intended semantics (and what CalDAV does) is to exclude by
> `resourceid`. Recommendation for the sidecar: use the `NOT EXISTS`/`resourceid`
> form below, and record the divergence as a deviation entry
> (`shared-book-group-unshare`) with a differential test, rather than
> replicating the CardDAV bug. Confirm against the deployed PHP before locking
> it in.

Recommended sidecar form (semantically correct, CalDAV-aligned):

```sql
SELECT a.id, a.uri, a.displayname, a.principaluri, a.description, a.synctoken, s.access
FROM oc_dav_shares s
JOIN oc_addressbooks a ON s.resourceid = a.id
WHERE s.principaluri IN (?, ?, ...)                 -- group principals + own principal
  AND s.type = 'addressbook'
  AND NOT EXISTS (
      SELECT 1 FROM oc_dav_shares d
      WHERE d.access = 5
        AND d.resourceid = s.resourceid
        AND d.principaluri IN (?, ?, ...)
  )
```

PHP adds no `ORDER BY`; result order is database order. The sidecar should add
`ORDER BY a.id` for determinism (see §7, ordering).

### 1.3 Group expansion in Rust

PHP expands groups through `IGroupManager` (can be LDAP/other backends) and
skips `hideFromCollaboration()` groups. The sidecar can only see the database
group backend. Query:

```sql
SELECT gu.gid
FROM oc_group_user gu
JOIN oc_groups g ON g.gid = gu.gid
WHERE gu.uid = ?
```

Each `gid` becomes a principal `principals/groups/<php_urlencode(gid)>`
(`Principal.php:207`). PHP `urlencode` differs from RFC 3986 percent-encoding:
space → `+`, `~` → `%7E`, `-_.` and alphanumerics unescaped. For the usual
ASCII-alphanumeric group id the two coincide; see §7 for the risk.

Principals bound into the query = `["principals/users/<uid>"] + group_principals`.

### 1.4 Wire name of a shared book

For every shared row (`CardDavBackend.php:172-185`):

```
name             = last path segment of a.principaluri        # owner uid, e.g. "bob"
wire_uri         = a.uri . "_shared_by_" . name               # e.g. "contacts_shared_by_bob"
wire_displayname = a.displayname . " (" . (owner_displayname ?? name) . ")"
owner-principal  = a.principaluri                             # e.g. "principals/users/bob"
read-only        = (s.access == 3)
principaluri     = <the sharee>                               # "principals/users/alice"
```

The **client PUTs to `.../addressbooks/users/<sharee>/<wire_uri>/<card>.vcf`**,
i.e. `<owner_book_uri>_shared_by_<owner_uid>` — not the owner's plain `uri`.

Resolution must be by **listing + name match**, not by parsing the suffix: Sabre's
`Collection::getChild()` iterates `getChildren()` and returns the first
`getName() === $name` (`3rdparty/sabre/dav/lib/DAV/Collection.php:getChild`;
`AddressBookHome` has no override). A book whose real URI already contains
`_shared_by_` therefore collides, and owned books win because they are listed
first. The sidecar should reproduce this: build the accessible list (owned +
shared), match the requested `book_uri` against the constructed wire names in
that order.

### 1.5 De-duplication (shared with a user twice, or user + group)

PHP keys `$addressBooks` by `oc_addressbooks.id` and applies, per shared row
(`CardDavBackend.php:155-171`):

1. `if ($row['principaluri'] === $principalUri) continue;` — `$row['principaluri']`
   is `a.principaluri` (owner), so a share of the user's *own* book is skipped
   (it is already in the owned list).
2. `$readOnly = (s.access === 3)`.
3. If the id is already present:
   * if the new row is read-only → **skip** (a read-only share never downgrades
     an existing entry);
   * if the existing entry has `read-only === false` → **skip** (an existing
     read-write entry never downgrades).
4. Otherwise insert/replace.

Net effect: **read-write wins regardless of row order**; a read-only duplicate
is dropped; a direct share and a group share of the same book collapse to one
entry (with `owner-principal`/`read-only` from the winning row). Rust should
reproduce this with an `IndexMap<i64, Book>` keyed by book id.

---

## 2. Properties — owned vs shared, read-only vs read-write

Property resolution is `src/routes.rs::resolve_property` /
`default_props`, serialised by `src/xml/write.rs`. `{DAV:}owner` is emitted as
`<d:href>{principal}/</d:href>`; Sabre resolves the relative principal against
the server base URI (`3rdparty/sabre/dav/lib/DAV/Xml/Property/Href.php:105`
`Uri\resolve($writer->contextUri, $href)`), so the sidecar's
`/remote.php/dav/principals/users/<uid>/` shape is right — but for a shared book
the uid must be the **owner**, not the caller.

| property | owned book | shared, read-write | shared, read-only | evidence |
|---|---|---|---|---|
| `{DAV:}displayname` | `oc_addressbooks.displayname` (sidecar falls back to `uri`) | `"<owner displayname> (<owner display name>)"` | same | `CardDavBackend.php:174` |
| `{DAV:}owner` | `<d:href>{caller principal}/</d:href>` | `<d:href>{owner principal}/</d:href>` | same | `AddressBook.php:191-198` (`getOwner`), `3rdparty/.../DAVACL/Plugin.php:1025` |
| `{http://nextcloud.com/ns}owner-displayname` | caller's display name | owner's display name | same | `CardDavBackend.php:1552-1567` |
| `{http://owncloud.org/ns}owner-principal` | **absent (404)** | `principals/users/<owner>` (plain text) | same | `CardDavBackend.php:184` |
| `{http://owncloud.org/ns}read-only` | **absent (404)** | present, **empty element** (`false` → `(string) false === ""`) | present, text `1` | `CardDavBackend.php:185`; `3rdparty/sabre/xml/lib/Serializer/functions.php` `is_scalar` → `(string)` |
| `{DAV:}current-user-privilege-set` | full set (below) | full set | `read`, `read-acl`, `read-current-user-privilege-set`, `write-properties` | `AddressBook.php:91-146`, `applyShareAcl` `DAV/Sharing/Backend.php:214-241` |
| `{http://owncloud.org/ns}invite` | lists shares (`oc:user`…) | **empty** `<oc:invite/>` | empty | `DAV/Sharing/Plugin.php:220-226`, `AddressBook.php:83-88` |
| `{DAV:}invite`, `{DAV:}share-access` | **404** | **404** | **404** | Sabre's `DAV\Sharing\Plugin` is *not* registered — Nextcloud registers `OCA\DAV\DAV\Sharing\Plugin` (`Server.php:208,231`) which is a bare `Sabre\DAV\ServerPlugin` and only handles `{oc}invite` |
| `{http://owncloud.org/ns}groups` | `CATEGORIES` distinct values | same (owner book's cards) | same | `CardDAV/Plugin.php:56` |
| `{carddav}addressbook-description`, `{cs}getctag`, `{sabredav}sync-token`, `{DAV:}sync-token` | owner book columns | **owner book columns** (same `a.*`) | same | `CardDavBackend.php:177-183` |

Full privilege set for an owned book and a read-write share (Sabre expands
`{DAV:}read` and `{DAV:}write` through `getFlatPrivilegeSet`,
`3rdparty/sabre/dav/lib/DAVACL/Plugin.php:437-484`, `:587-667`):

```
read, read-acl, read-current-user-privilege-set,
write, write-properties, write-content, unlock, bind, unbind, write-acl
```

Read-only share — `AddressBook::getACL()` adds `{DAV:}read` for the sharee
(`AddressBook.php:126-131`) and `applyShareAcl` adds `{DAV:}write-properties`
for a read-only calendar/addressbook share (`DAV/Sharing/Backend.php:232-240`),
then aggregates `read`:

```
read, read-acl, read-current-user-privilege-set, write-properties
```

`{oc}invite` serialisation (owned books), for reference
(`apps/dav/lib/DAV/Sharing/Xml/Invite.php:108-140`): each share is
`<oc:user><d:href>principal:<principaluri></d:href><oc:common-name>…</oc:common-name><oc:invite-accepted/><oc:access><oc:read|oc:read-write/></oc:access></oc:user>`.
The sidecar currently emits none of this; shared books only need the empty
element for parity, but owned books are already divergent.

**Model implication:** `src/model.rs::AddressBook` needs the owner principal and
the read-only flag. Suggested addition:

```rust
pub struct AddressBook {
    pub id: i64,
    pub uri: String,              // wire name (owned uri, or "<uri>_shared_by_<owner>")
    pub displayname: Option<String>, // for shared: already "<name> (<owner>)"
    pub principaluri: String,     // sharee principal (what the node reports)
    pub description: Option<String>,
    pub synctoken: i64,
    pub owner_principal: Option<String>, // Some => shared; {DAV:}owner + {oc}owner-principal
    pub read_only: Option<bool>,         // Some => shared; {oc}read-only
    pub owner_displayname: Option<String>, // {nc}owner-displayname
}
```

`PropContext.principal_href` / `owner_displayname` are currently built from the
*request* user once per request (`src/routes.rs:464-473`, `:1100-1109`,
`:1252-1261`). They must become per-book: `owner_principal` when set, else the
caller; `owner_displayname` likewise. `resolve_property` must then add
`{oc}owner-principal`, `{oc}read-only` (empty text for `false`), and the
read-only privilege set.

---

## 3. Writes — what a `PUT`/`DELETE` to a shared book does

### 3.1 Which book the card lands in

Sabre's `AddressBook` node carries `addressBookInfo['id']` = the **owner's**
`oc_addressbooks.id` (the listing copies `a.id`). `createFile`/`put` call
`createCard($ownerBookId, …)` / `updateCard($ownerBookId, …)`
(`3rdparty/sabre/dav/lib/CardDAV/AddressBook.php`), so:

* `oc_cards.addressbookid` = owner book id;
* `oc_addressbookchanges` + `oc_addressbooks.synctoken` bump on the owner book;
* `oc_cards_properties` keyed by owner book id;
* the event is `CardCreatedEvent`/`CardUpdatedEvent`/`CardDeletedEvent`
  with `$addressBookId = owner book id` (`CardDavBackend.php:689`, `:752`,
  `:836`).

The sidecar's `put_card`/`delete_card` already take an `address_book_id`; once
the shared book resolves to the owner's row, **no write-path change is needed**
for the data itself — `book.id` is the owner id.

### 3.2 ACL enforced

Sabre checks, before the handler (`3rdparty/sabre/dav/lib/DAVACL/Plugin.php`
`beforeMethod`):

* `PUT` → `{DAV:}write-content` on the card (and `{DAV:}bind` on the parent
  when creating);
* `DELETE` card → `{DAV:}unbind` on the parent book;
* `GET`/`HEAD` → `{DAV:}read`.

Read-only share: the sharee's ACL lacks `write`, so the check fails.
`DavAclPlugin::checkPrivileges` (`apps/dav/lib/Connector/Sabre/DavAclPlugin.php:42-68`)
then throws **403 only when `getCurrentUserPrincipal() === $node->getOwner()`**,
else **404**. For a shared book `getOwner()` is the *owner* principal
(`AddressBook.php:191-198`, `Card.php:38-44`), so a read-only sharee always gets
**404** — never 403. Read-write share: allowed. Owner: allowed.

`DELETE` on the whole shared book is *not* a card write: `AddressBook::delete()`
(`AddressBook.php:201-216`) unshares (removes the direct share) and throws
`Forbidden` if only a group share exists. The sidecar answers 501 for collection
DELETE (`src/routes.rs` `DavTarget::Book` `_ => not_implemented()`), so this
stays on PHP.

### 3.3 Outbox / event effects

The outbox row must carry the **owner book id** (`addressbookid`), exactly like a
personal write; the PHP worker then re-reads `getShares($addressBookId)` and
dispatches the event with the correct share list
(`app/nextcloud_dav/lib/Command/EventDispatch.php:299-331`). No new outbox column
is required for the *data*.

**Actor gap.** `CardDavBackend` events carry no actor; the activity listener
derives the author from the current session user, falling back to the owner
(`docs/recon/card-events.md` §1.1). The outbox worker runs under `occ` with no
session user, so a sharee's write is already recorded as the **owner's** action
even today (for personal writes owner == actor, so it is invisible). For shared
books this becomes user-visible: `PUT` by alice into bob's shared book would be
attributed to bob. Fix options: (a) add an `actor_uid` column to
`oc_dav_event_outbox` + companion migration and have the worker set a session
user before `dispatchTyped`; (b) accept it and document. This is a prerequisite
for correct shared-book activity, not optional.

### 3.4 What the current sidecar would get wrong

* `address_book_by_uri()` (`src/db.rs:91-110`) scopes to
  `principaluri = 'principals/users/<uid>'`, so a shared book resolves to
  `None` → **404**, not 501, so nginx does **not** replay it to PHP
  (ARCHITECTURE §3 intercepts only 501/502/504). Today that means shared books
  are broken at the sidecar rather than delegated — verify the deployed nginx
  config; if it only routes personal paths, this is moot, otherwise it is a live
  bug.
* No ACL: the sidecar would happily serve and write a shared book for any caller
  that can name it, including a read-only sharee.
* `{DAV:}owner`, `{nc}owner-displayname`, `{oc}owner-principal`,
  `{oc}read-only`, `current-user-privilege-set` are all computed from the caller.
* `put_card`/`delete_card` pass `book.id` through unchanged — correct once
  resolution is correct.
* No actor in the outbox (§3.3).

---

## 4. ACL / privacy rule, 404 vs 403

Define `access(book, user)`:

```
owned(book)  := book.principaluri == 'principals/users/<user>'
shared_rw    := ∃ share row (type='addressbook', resourceid=book.id,
                             access=2, principaluri ∈ user_principals)
shared_ro    := ∃ share row (… access=3 …)
unshared     := ∃ tombstone (… access=5, resourceid=book.id,
                             principaluri ∈ user_principals)
visible(book):= (owned or shared_rw or shared_ro) and not unshared
writable(book):= owned or shared_rw
```

* **Read** (`GET`/`HEAD`/`PROPFIND`/`REPORT`): `visible(book)` → serve; else 404.
* **Write a card** (`PUT`/`DELETE`): `writable(book)` → serve; else
  **404** (never 403 — the caller is not the node owner; DavAclPlugin hides
  existence). 404 also for `!visible`.
* **Write the collection** (DELETE/PROPPATCH/MKCOL): 501 → PHP (which returns
  403/404 per its own logic).
* Path principal ≠ authenticated user → 404 (already in `src/routes.rs:345-350`,
  `:369-373`, `:394-398`).
* Unknown / not-visible card or book → 404.

The single exception where PHP returns 403 is when the caller *is* the owner and
the ACL denies (e.g. system book write); the sidecar does not serve system books,
so it can treat all ACL denials as 404.

The group expansion in the visibility check must mirror the listing: groups via
`oc_group_user`/`oc_groups`, urlencoded principals, hidden-from-collaboration
groups excluded (§7), tombstones honoured.

---

## 5. Home listing (future Rust)

The sidecar does **not** serve `/remote.php/dav/addressbooks/users/<u>/` today
(nginx keeps it on PHP; `src/routes.rs` has a `DavTarget::Home` handler reachable
only on misroute). A future Rust home listing must emit, in this order, to match
PHP byte-for-byte (`UserAddressBooks.php:59-112`):

1. **Owned books** — one response per `oc_addressbooks` row
   (`principaluri = principals/users/<uid>`).
2. **Shared books** — one response per de-duplicated shared book (§1.5), href
   `<home>/<uri>_shared_by_<owner>/`, with the §2 shared properties.
3. **System book** — appended only when
   `oc_appconfig['dav']['system_addressbook_exposed']` is true **and** the home
   is not `principals/system/system` (`UserAddressBooks.php:66-73`). Href
   `…/z-server-generated--system/` (`SystemAddressbook::URI_SHARED`), displayname
   `"Accounts"` (`SystemAddressbook.php:46`), owner `principals/system/system`,
   `{oc}owner-principal = principals/system/system`, `{oc}read-only = 1`,
   description `"System address book which holds all accounts"`. Its cards are
   filtered by share-dialog enumeration settings (`SystemAddressbook.php:58-100`)
   — the sidecar must **not** claim it.
4. **App-generated books** from `IAddressBookProvider`
   (`UserAddressBooks.php:104-111`): `z-app-generated--contactsinteraction--recent`
   (`contactsinteraction/lib/AddressBook.php:28`, displayname
   `"Recently contacted"`, `{oc}read-only = 1`, no owner-principal, no IACL).
   The sidecar must **not** claim these either.

So a Rust home listing is only "PHP-identical" if it either reproduces the
system/contactsinteraction nodes or keeps them on PHP. Since both are
config/app-dependent and the contactsinteraction book is a virtual collection
backed by `oc_recent_contact` (not `oc_cards`), the recommendation is to keep
serving the home on PHP and only implement the *book/card* paths, which is what
the shared-book work actually needs.

If the home is ever implemented, the multistatus must also carry the home
collection's own `{DAV:}resourcetype`, `{DAV:}current-user-privilege-set`,
`{DAV:}owner`, `{DAV:}supported-report-set` as today (`default_props`,
`src/routes.rs:684-694`).

---

## 6. Implementation plan for the sidecar

### 6.1 Queries (`src/db.rs`)

1. `group_principals(uid) -> Vec<String>` — the `oc_group_user`/`oc_groups`
   query of §1.3, mapping each gid through a `php_urlencode` helper.
2. Extend `address_books_for_user` to return owned + shared in one ordered list
   (owned query, then the shared query of §1.2 with the principal list bound
   twice). Fold in Rust with an `IndexMap<i64, AddressBook>` reproducing §1.5.
   Compute `uri`, `displayname`, `owner_principal`, `read_only` exactly as §1.4.
3. Add `address_book_for_user(uid, wire_uri) -> Option<AddressBook>` that runs
   the listing and returns the first wire-name match (§1.4), replacing the
   two `address_book_by_uri(principal, uri)` call sites used for serving. Keep
   `address_book_by_uri` for the owned fast path if desired, but the shared path
   must go through the listing.
4. `user_display_name(owner_uid)` already exists (`src/db.rs:250-268`) and gives
   the §1.4 owner display name (fallback uid).

### 6.2 Model (`src/model.rs`)

Add `owner_principal`, `read_only`, `owner_displayname` to `AddressBook`
(§2). Keep `principaluri` = sharee for owned and shared (PHP sets it to the
sharee, `CardDavBackend.php:180`).

### 6.3 Properties (`src/routes.rs`)

* Build `PropContext` per book, not per request: `principal_href` from
  `owner_principal` when set; `owner_displayname` from the book.
* `resolve_property`:
  * `{oc}owner-principal` → `PropValue::Text(owner_principal)` when shared,
    else `None` (404);
  * `{oc}read-only` → `PropValue::Text("1")` / `PropValue::Text("")` when
    shared, else `None`;
  * `{oc}invite` → `PropValue::Elements(vec![])` (empty element) — needed for
    parity at least for shared books; owned-book share lists are a separate
    task;
  * `{DAV:}current-user-privilege-set` → the read-only set of §2 when
    `read_only == Some(true)`, else the existing full set.
* `default_props` for a book should include the new properties so `allprop`
  matches (currently absent).
* `handle_propfind`, `handle_report_book`, `handle_report_card`, `get_card`,
  `put_card`, `delete_card` must all resolve through the shared-aware lookup and
  must pass the per-book context.

### 6.4 Write ACL (`src/routes.rs`)

* After resolving the book, if `read_only == Some(true)` (or the caller has no
  visible access) return **404** for `PUT`/`DELETE` before reading the body /
  before any DB write.
* Owned and read-write shared books proceed as today, writing to the owner's
  `book.id`.
* Consider adding the `actor_uid` to the outbox (§3.3) — separate companion-app
  migration.

### 6.5 Tests

* Extend `tests/common/mod.rs` schema with `oc_group_user` and `oc_groups`, and
  add `seed_group`/`seed_group_member`/`seed_share` helpers (the schema already
  has `oc_dav_shares`, `tests/common/mod.rs:760`).
* `db_read_path.rs`: listing — owned only; direct share; group share; user +
  group de-dup (read-write wins both orders); read-only vs read-write; owner
  self-share skipped; tombstone hides a direct share; tombstone vs group share
  (pin the chosen `resourceid` semantics); wire name and displayname.
* `http_read.rs`: PROPFIND on `contacts_shared_by_bob` — `{DAV:}owner`,
  `{nc}owner-displayname`, `{oc}owner-principal`, `{oc}read-only`,
  `current-user-privilege-set`; `GET` a card; `PUT` allowed on rw, **404** on ro;
  `DELETE` allowed on rw, 404 on ro; a non-sharee gets 404.
* `deviations.rs`: flip `shared-books-php` to `resolved` and pin the new
  behavior; add `shared-book-group-unshare` (the `resourceid` vs `s.id`
  divergence) and, if `ORDER BY` is added, `shared-book-listing-order`.
* Differential conformance (`tests/conformance/conformance.py`) against PHP for
  a shared book's PROPFIND byte parity, if a live instance is available.

### 6.6 Deviation entries (`tests/deviations.toml`)

* `shared-books-php` → `status = "resolved"`, update `nextcloud`/`sidecar`/
  `why`/`test`/`evidence` to the new behavior.
* add `shared-book-group-unshare` (`status = "intentional"` or
  `"likely-wrong"`) documenting the deliberate use of `resourceid` tombstones
  where CardDAV PHP uses `s.id`.
* keep `home-listing-php` and `contactsinteraction-php` unchanged (the home and
  the app-generated books still stay on PHP).
* if an `actor_uid` column is added, note it in the outbox schema/deviation set.

---

## 7. Risks / unknowns

1. **Group expansion is not DB-only.** PHP uses `IGroupManager`; LDAP (and other
   backends) store membership outside `oc_group_user`, and
   `hideFromCollaboration()` is backend state with **no `oc_groups` column**
   (`lib/private/Group/Group.php:397-402`). A pure-SQL expansion will list books
   shared with LDAP groups incorrectly (miss them) and may include
   hidden-from-collaboration groups. Mitigations: detect LDAP/appconfig and fall
   back to PHP for the listing, or store a `dav` marker. This is the single most
   important unknown.
2. **`urlencode` vs percent-encoding and group-principal storage.** `getGroupMembership`
   urlencodes the gid; `dav_shares.principaluri` is whatever
   `Principal::findByUri` returned, which may be encoded or not
   (`Principal.php:155-163`). They agree for `[A-Za-z0-9]` gids and can disagree
   for spaces/`+`/`~`. Needs a fixture with a special-character group id.
3. **The CardDAV `s.id` tombstone bug.** Matching PHP literally reproduces a
   bug; matching CalDAV diverges from PHP for group-unshare. Decide with a
   differential test. (See §1.2.)
4. **Actor attribution.** No actor in the outbox → shared-book writes are
   attributed to the owner (§3.3). Needs a schema change to fix.
5. **Listing order.** PHP has no `ORDER BY`; a deterministic `ORDER BY a.id`
   may reorder the multistatus vs PHP. Affects only response order, except for
   the owned-vs-shared wire-name collision (owned must stay first).
6. **`{DAV:}current-user-privilege-set` order.** Sabre builds it by a stack DFS
   (`3rdparty/sabre/dav/lib/DAVACL/Plugin.php:641-667`); the sidecar hardcodes an
   order. The read-only set's membership is certain, its byte order is not;
   verify against PHP.
7. **`{oc}invite`** is currently unserved for owned books too; adding the empty
   element for shared books is safe, but a full parity fix is a separate task.
8. **nginx fallback for 404.** If the deployed nginx routes shared paths to the
   sidecar and the sidecar returns 404 (not 501), those requests never reach PHP
   (ARCHITECTURE §3). This must be re-checked in the infra repo before shipping;
   it is a correctness risk independent of the new code.
9. **Not determinable from source here:** the actual contents of the deployed
   nginx config (lives in the infra repo), whether the target instance uses LDAP
   or circles for the affected users, and whether the `system_addressbook_exposed`
   config is on. The `{oc}invite` order and the exact multistatus element order
   also need a live PHP capture.
