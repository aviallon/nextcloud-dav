# Recon: side effects of `CardCreatedEvent` / `CardUpdatedEvent` / `CardDeletedEvent`

Repo: `nextcloud-server` (Nextcloud 36-dev, HEAD `d077e686132`), read at
`/home/aviallon/Programing/Opensource/nextcloud/nextcloud-server`.
Target of the question: `nextcloud-dav` (read-only Rust CardDAV sidecar,
`docs/ARCHITECTURE.md`).

All `file:line` references below are relative to `nextcloud-server` unless
explicitly marked **external**.

---

## 0. Dispatch and transaction semantics

The three events are dispatched from `apps/dav/lib/CardDAV/CardDavBackend.php`,
**inside** the `TTransactional::atomic()` closure of the write:

| event | dispatch line | writes before dispatch |
|---|---|---|
| `CardCreatedEvent` | `CardDavBackend.php:689` | `oc_cards` INSERT (`:668`), `addChange` op 1 (`:683`), `updateProperties` (`:684`) |
| `CardUpdatedEvent` | `CardDavBackend.php:752` | `oc_cards` UPDATE (`:734`), `addChange` op 2 (`:746`), `updateProperties` (`:747`) |
| `CardDeletedEvent` | `CardDavBackend.php:836` | `oc_cards` DELETE (`:827`), `addChange` op 3 (`:832`) |

`atomic()` calls `beginTransaction()` unconditionally
(`lib/public/AppFramework/Db/TTransactional.php:44`). Nextcloud enables
`setNestTransactionsWithSavepoints(true)` (`lib/private/DB/Connection.php:191`),
so every listener that itself calls `atomic()` runs in a savepoint of the same
outer transaction. **All listener DB writes commit together with the card
write.**

Exceptions are **not** swallowed by the dispatcher:
`lib/private/EventDispatcher/EventDispatcher.php:72` is a thin wrapper over
Symfony's dispatcher (`:74`) with no try/catch. Therefore:

- `CardListener` catches `Throwable` itself → activity failures are non-fatal.
- `BirthdayListener` and `ClearPhotoCacheListener` have **no** try/catch →
  a throwing birthday/photo-cache operation propagates out of `atomic()`,
  rolls back the card write, and fails the HTTP request.

Listener registrations: `apps/dav/lib/AppInfo/Application.php:188-195`.

---

## 1. `CardListener` → `OCA\DAV\CardDAV\Activity\Backend::triggerCardActivity`

- `apps/dav/lib/Listener/CardListener.php:33-95` (three branches; each wrapped
  in `try { … } catch (Throwable)` at `:34/:45`, `:52/:63`, `:70/:81`).
- `apps/dav/lib/CardDAV/Activity/Backend.php:375` `triggerCardActivity`.

### 1.1 Who is notified

1. Skip if `$addressbookData['principaluri']` is unset (`Backend.php:376-378`).
2. Skip the system addressbook `principals/system/system` (`Backend.php:381-384`).
3. Owner = last path segment of `principaluri` (`Backend.php:386-387`).
4. Author = current session user, else owner (`Backend.php:389-394`).
5. Recipients = owner + every user/group share of the address book
   (`Backend.php:405-406`, `getUsersForShares()` at `:448`, group members
   expanded through `IGroupManager`; shares come from `oc_dav_shares` via
   `CardDavBackend::getShares()` `apps/dav/lib/CardDAV/CardDavBackend.php:1406`).
6. `card.id` = vCard `UID`, `card.name` = `FN` (`Backend.php:437-439`, using
   Sabre `Reader::read`). **This is a PHP VObject parse.**
7. Per recipient: subject = `<action>_self` if recipient == author else
   `<action>`; `action ∈ {card_add, card_update, card_delete}`
   (`Backend.php:410-429`; constants `Activity/Provider/Card.php:22-24`).
8. `$this->activityManager->publish($event)` (`Backend.php:429`).

Event object built at `Backend.php:400-403`:
`setApp('dav')`, `setObject('addressbook', <addressbook id>)`,
`setType('contacts')`, `setAuthor($currentUser)`.

### 1.2 What is written, and by whom

`OCP\Activity\IManager` is implemented by `OC\Activity\Manager`
(`lib/private/Activity/Manager.php`). `publish()` (`:105`) fills in
`timestamp` (now) if unset and then fans out to registered consumers
(`:118`). **The Activity app is not part of this git checkout** (there is no
`apps/activity` directory). It ships as a separate repository/app, and it is
the component that owns `oc_activity` / `oc_activity_mq` and registers the
consumer.

Evidence below is from the **external** `nextcloud/activity` repo (master),
which may differ from the deployed version — see §7.

Consumer `Consumer::receive()` **external** `lib/Consumer.php:42-59`:
- always `Data::send($event)`;
- if recipient ≠ author and push setting on →
  `NotificationGenerator::sendNotificationForEvent()`;
- if recipient ≠ author and email setting on → `Data::storeMail()`.

`Data::send()` **external** `lib/Data.php:94-139`:
- gate `shouldSend()` (`:57`): affecteduser non-empty and author not in system
  config `activity_log_exclude_users`;
- `INSERT INTO oc_activity` with columns
  `app, subject, subjectparams, message, messageparams, file, link, user,
  affecteduser, timestamp, priority, type, object_type, object_id`
  (`:101-137`). Mapping for a card event:
  - `app='dav'`, `type='contacts'`
  - `user` = author, `affecteduser` = recipient
  - `timestamp` = `time()` (Unix seconds)
  - `subject` = `card_add|card_add_self|card_update|card_update_self|card_delete|card_delete_self`
  - `subjectparams` = `json_encode($event->getSubjectParameters())` (`:128`) —
    **default `json_encode` flags**, i.e. `/` → `\/` and non-ASCII → `\uXXXX`
  - `message=''`, `messageparams='[]'`
    (`lib/private/Activity/Event.php:41,43` defaults; `getMessageParameters()=[]`)
  - `file=''`, `link=''` (never set by `triggerCardActivity`)
  - `priority=30` (`IExtension::PRIORITY_MEDIUM`, `lib/public/Activity/IExtension.php:45`)
  - `object_type='addressbook'`, `object_id=<addressbook id>`
- subjectparams JSON, key order as inserted:
  `{"actor":…,"addressbook":{"id":<int>,"uri":…,"name":…},"card":{"id":<UID>,"name":<FN>}}`
  (`Backend.php:410-423`).

`Data::storeMail()` **external** `lib/Data.php:222-256`:
- `INSERT INTO oc_activity_mq` with columns
  `amq_appid, amq_subject, amq_subjectparams, amq_affecteduser, amq_timestamp,
  amq_type, amq_latest_send, object_type, object_id`.
- `amq_latest_send = event timestamp + batchtime`.

### 1.3 Settings consulted

- Stream: **not consulted before insert.** The row is always written; the
  stream filter is applied on read (`Data::applyStreamConditions`).
- Email/push: `OCA\Activity\UserSettings::getUserSetting()`
  **external** `lib/UserSettings.php:54-84`:
  - user value from `oc_preferences` where `appid='activity'`,
    `configkey='notify_<method>_<type>'` (`method ∈ {email,notification,setting}`,
    `type='contacts'` here);
  - admin default from `oc_appconfig` `appid='activity'`,
    `configkey='notify_<method>_<type>'`;
  - global kill-switch `oc_appconfig` `activity/enable_email` (`:55`).
- For CardDAV `Setting` (`apps/dav/lib/CardDAV/Activity/Setting.php`), which
  extends `CalDAVSetting` → `ActivitySettings`:
  `isDefaultEnabledMail()=false`, `canChangeMail()=true`,
  `isDefaultEnabledNotification() = isDefaultEnabledMail() && !canChangeMail()`
  = **false** (`lib/public/Activity/ActivitySettings.php:81-82`). So push is
  **off by default**, opt-in per user. Stream default is on
  (`Setting.php:45-70`).
- There is **no `oc_activity_settings` table** in the Activity app (checked all
  12 files in `nextcloud/activity` `lib/Migration`; only `activity` and
  `activity_mq` are created). Settings live in `oc_preferences`/`oc_appconfig`.

### 1.4 Redis / push

- The activity consumer itself writes **only to the DB** (no `ICache`).
- Push is a *different* app: `NotificationGenerator::sendNotificationForEvent()`
  **external** `lib/NotificationGenerator.php:53-58` calls
  `OCP\Notification\IManager::notify()`, implemented by the notifications app
  (not in this checkout) → `oc_notifications` row + `notify_push`/Redis push.
  Disabled by default for `contacts` (see above).
- Mail is queued in `oc_activity_mq`; a background job sends it later.

**Activity verdict:** one `oc_activity` row per recipient, in the card-write
transaction; optionally one `oc_activity_mq` row and/or one notification.
No Redis write from this path.

---

## 2. `BirthdayListener` → `OCA\DAV\CalDAV\BirthdayService`

- `apps/dav/lib/Listener/BirthdayListener.php:27-39`: created/updated →
  `onCardChanged(addressBookId, uri, carddata)`; deleted →
  `onCardDeleted(addressBookId, uri)`. **No try/catch.**
- `apps/dav/lib/CalDAV/BirthdayService.php`.

### 2.1 Flow

`onCardChanged` (`BirthdayService.php:49-83`):
1. global gate: `oc_appconfig` `dav/generateBirthdayCalendar == 'yes'`
   (default yes) (`:52`, `isGloballyEnabled()` `:377-379`);
2. `getAllAffectedPrincipals()` (`:308-323`): sharees from
   `CardDavBackend::getShares()` (owner + user shares; group shares expanded via
   `GroupPrincipalBackend::getGroupMemberSet` → `oc_group_user`, `:313`), plus
   the address book owner (`:61`);
3. per principal: per-user gate `oc_preferences` `dav/generateBirthdayCalendar`
   (default yes) (`:69`, `isUserEnabled()` `:400-405`);
4. reminder offset `oc_preferences` `dav/birthdayCalendarReminderOffset`
   (default `PT9H`) (`:73`, `getReminderOffsetForUser()` `:418-421`);
5. `ensureCalendarExists()` (`:110-123`): `SELECT … FROM oc_calendars WHERE
   uri='contact_birthdays' AND principaluri=?` (`CalDavBackend.php:633`);
   if missing, `createCalendar()` (`CalDavBackend.php:818`, INSERT into
   `oc_calendars` with `synctoken=1`, displayname "Contact birthdays",
   colour `#E9D859`, components `VEVENT`);
6. for each of `BDAY`, `DEATHDATE` (`-death`), `ANNIVERSARY` (`-anniversary`)
   (`:64-68`): `updateCalendar()` (`:334-368`).

`onCardDeleted` (`:85-108`): same gates; computes
`objectUri = <addressbook uri>-<card uri><tag>.ics` and hard-deletes the three
objects with `deleteCalendarObject(..., forceDeletePermanently: true)`
(`:102`, `CalDavBackend.php:1769`). Because `forceDeletePermanently=true`, the
trashbin path is skipped and the row is deleted from `oc_calendarobjects`.

### 2.2 How the VEVENT is built

`buildDateFromContact()` (`:132-247`):
- Sabre `Reader::read`, `$doc->convert(Document::VCARD40)` (`:143-147`);
- skip if `X-NC-EXCLUDE-FROM-BIRTHDAY-CALENDAR` (`:149`), if the field is
  absent (`:152`), if `FN` absent (`:155`), if the value is empty (`:158`), or
  not a `DateAndOrTime` (`:161`);
- skip unparsable values (`:164-168`);
- year handling: `X-APPLE-OMIT-YEAR` == year, or literal year 1604 → year
  dropped (`:169-177`); unknown year → `1970-<m>-<d>` (or `1972-` for Feb 29)
  (`:186-188`);
- SUMMARY via `formatTitle()` (`:190`, `:437-493`): `🎂 <FN> (<year>)`,
  `Death of %s`, `💍 <FN> (<year>)`; **l10n** for DEATHDATE; 4-byte-text
  fallback when `dbConnection->supports4ByteText()` is false;
- serialization (`:192-240`): `VCalendar` `VERSION=2.0`,
  `PRODID=-//IDN nextcloud.com//Birthday calendar//EN`; `VEVENT` with
  `DTSTART;VALUE=DATE`, `DTEND;VALUE=DATE` (= start + 1 day),
  `UID = <vcard UID><postfix>`, `RRULE=FREQ=YEARLY` (Feb 29:
  `FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=-1`), `SUMMARY`, `TRANSP=TRANSPARENT`,
  `X-NEXTCLOUD-BC-FIELD-TYPE`, `X-NEXTCLOUD-BC-UNKNOWN-YEAR`,
  `X-NEXTCLOUD-BC-YEAR`; optional `VALARM` (`TRIGGER;VALUE=DURATION`,
  `ACTION=DISPLAY`, `DESCRIPTION=SUMMARY`) when a reminder offset exists.

`updateCalendar()` (`:334-368`):
- object URI = `<addressbook uri>-<card uri><postfix>.ics`;
- if the card no longer yields a VEVENT and an object exists → delete;
- if the object does not exist by URI, look it up by UID
  (`getCalendarObjectByUID` `CalDavBackend.php:2678`, to handle address-book
  moves) and delete the stale one, then `createCalendarObject`;
- else if `birthdayEvenChanged()` (DTSTART or SUMMARY differs, `:317-331`) →
  `updateCalendarObject`.

### 2.3 Tables touched (all inside the card-write transaction)

Reads: `oc_addressbooks`, `oc_dav_shares`, `oc_group_user`, `oc_calendars`,
`oc_calendarobjects`, `oc_calendarobjects_props`, `oc_appconfig`,
`oc_preferences`.

Writes:
- `oc_calendars` — INSERT when creating the birthday calendar
  (`CalDavBackend.php:858`); `synctoken` UPDATE via `addChanges`
  (`CalDavBackend.php:3356`);
- `oc_calendarobjects` — INSERT/UPDATE/DELETE
  (`CalDavBackend.php:1550/1628/1769`; columns `calendarid, uri, calendardata,
  lastmodified, etag, size, componenttype, firstoccurence, lastoccurence,
  classification, uid, calendartype, deleted_at`);
- `oc_calendarobjects_props` — `purgeProperties` + INSERT of indexed props
  (`CalDavBackend.php:3711-3760`, `:3914`; `INDEXED_PROPERTIES` `:181-193`,
  including `SUMMARY`);
- `oc_calendarchanges` — INSERT with the **pre-increment** synctoken
  (`CalDavBackend.php:3340-3357`), then `oc_calendars.synctoken += 1`;
- `oc_calendar_invitations` — `purgeObjectInvitations` on delete
  (`CalDavBackend.php:4107`).

### 2.4 Cascading events — the hidden cost

`createCalendarObject` / `updateCalendarObject` / `deleteCalendarObject`
dispatch calendar events (`CalDavBackend.php:1595`, `:1678`, `:1795`), which
have their own listeners (`Application.php:153-170`):

- `ActivityUpdaterListener` (`apps/dav/lib/Listener/ActivityUpdaterListener.php:129-146`
  create, `:220-240` delete) → `CalDAV\Activity\Backend::onTouchCalendarObject`
  (`apps/dav/lib/CalDAV/Activity/Backend.php:402`):
  - app `dav`, type `calendar_event`, object `('calendar', <birthday calendar id>)`;
  - subject `event_add`/`event_add_self` / `event_delete`/`event_delete_self`;
  - subjectparams `{actor, calendar:{id,uri,name}, object:{id=<UID>,
    name=<SUMMARY>, classified=false}}`, plus `object.link`
    `{object_uri, calendar_uri, owner}` when the `calendar` app is enabled
    (`:462-467`);
  - recipients = owner + calendar shares (birthday calendars are not shared,
    so effectively the owner);
  - → more `oc_activity` rows (and potentially `oc_activity_mq`).
- `CalendarObjectReminderUpdaterListener`
  (`apps/dav/lib/Listener/CalendarObjectReminderUpdaterListener.php:93-107`
  create, `:153-168` delete) → `ReminderService::onCalendarObjectCreate/Delete`
  (`apps/dav/lib/CalDAV/Reminder/ReminderService.php:173`, `:332`):
  - parses the VEVENT, expands recurrences (`EventIterator` at `:240`,
    `getRemindersForVAlarm` `:351`, loop `:250-306`), and INSERTs into
    `oc_calendar_reminders` (`Reminder/Backend.php:132-160`); delete →
    `cleanRemindersForEvent` (`:199`, DELETE from `oc_calendar_reminders`);
  - this is the reason birthday events generate reminders at all.
- `CalendarContactInteractionListener` is also registered for
  `CalendarObjectCreatedEvent` (`Application.php:163`) but is a no-op for
  birthday events: the birthday calendar has no shares and the VEVENT has no
  `ATTENDEE` (`apps/dav/lib/Listener/CalendarContactInteractionListener.php:52-76`).
- `createCalendar` dispatches `CalendarCreatedEvent` **after** its own
  `atomic()` (`CalDavBackend.php:869`), which generates a `calendar_add`
  activity row when the birthday calendar is first created
  (`ActivityUpdaterListener.php:44-55`).

**Birthday verdict:** a single contact with a BDAY mutates up to ~7 tables
(`oc_calendarobjects`, `oc_calendarobjects_props`, `oc_calendarchanges`,
`oc_calendars.synctoken`, `oc_calendar_reminders`, `oc_activity`, plus
`oc_calendar_invitations` on delete), and the object bytes and reminder rows
are produced by Sabre VObject code.

---

## 3. `ClearPhotoCacheListener` → `PhotoCache::delete`

- `apps/dav/lib/Listener/ClearPhotoCacheListener.php:27-31`: only
  `CardUpdatedEvent` and `CardDeletedEvent` (registered
  `Application.php:194-195`; a newly created card has no cache yet). No
  try/catch.
- `apps/dav/lib/CardDAV/PhotoCache.php:267-274` `delete()`.

Location and naming:
- App data instance = `IAppDataFactory::get('dav-photocache')`
  (`PhotoCache.php:277-280`).
- `IAppData` folder name = `appdata_<instanceid>` (`lib/private/Files/AppData/AppData.php:39-48`),
  then the app id → node path `appdata_<instanceid>/dav-photocache`.
- Per-card folder = `md5($addressBookId . ' ' . $cardUri)` (`PhotoCache.php:151`)
  — note the **single space** separator.
- Files inside: `nophoto` (marker), `photo.<ext>` where
  `ext ∈ {png,jpg,gif,ico,webp,avif}` (`:37-44`, `:78-90`), and derived
  `photo.<size>.<ext>` thumbnails (`:108-130`).
- `delete()` = `getFolder(..., createIfNotExists:false)->delete()`, i.e. a
  recursive node delete; `NotFoundException` is swallowed (`:270-273`).

Storage backend:
- appdata is a normal node in the root storage. On a local instance the
  physical path is `<datadirectory>/appdata_<instanceid>/dav-photocache/<md5>/`.
- On object store, `ObjectStoreStorage` uses object keys `urn:oid:<fileid>`
  (`lib/private/Files/ObjectStore/ObjectStoreStorage.php:46,262`) and the
  path→fileid mapping lives in `oc_filecache`; `oc_storages` identifies the
  backend.
- Deleting the folder through the files API removes both the `oc_filecache`
  rows and the physical file / object.

**Photo-cache verdict:** the identity of what to delete is fully derivable
(`md5("<addressBookId> <cardUri>")`), but doing it correctly means going
through `oc_filecache` (+ object store), not just `rm -rf`.

---

## 4. Other listeners for these three events

Exhaustive search over `apps/`, `lib/`, `core/` for the three class names:

| site | kind | effect |
|---|---|---|
| `apps/dav/lib/AppInfo/Application.php:188-195` | 8 registrations | the three listeners above |
| `apps/dav/lib/CardDAV/CardDavBackend.php` | dispatch | §0 |
| `apps/dav/lib/Events/Card{Created,Updated,Deleted}Event.php` | event classes | plain `OCP\EventDispatcher\Event`, **do not** implement `IWebhookCompatibleEvent` |
| `lib/private/Federation/CloudIdManager.php:42,54-67` | `addListener(CardUpdatedEvent::class, …)` in the constructor | Redis cache invalidation |

### 4.1 Why `CloudIdManager` matched

`OC\Federation\CloudIdManager::__construct()` (`:37-43`) registers
`handleCardEvent` for `CardUpdatedEvent` only. `handleCardEvent` (`:54-67`)
scans the raw carddata line by line for lines starting with `CLOUD;`, splits on
the first `:`, and for each value calls `unset($this->cache[$key])` and
`$this->memCache->remove($key)`. `$this->memCache` is a distributed
`ICache` (`createDistributed('cloud_id_')`, `:39`), i.e. **Redis** on a normal
deployment. So this is the only Redis side effect of the three events: a `DEL`
of `cloud_id_<cloudId>` for every `CLOUD;…:` line in an updated vCard
(and only if the `CloudIdManager` service was actually instantiated in that
request, since the listener is registered in its constructor).

Note the constructor also builds `displayNameCache` (`cloudid_name_`, `:40`)
but `handleCardEvent` does **not** touch it.

### 4.2 Checked and not applicable

- `webhook_listeners`: `WebhooksEventListener` is only registered for event
  classes configured by an admin, and `WebhookListenerMapper` rejects classes
  that do not implement `IWebhookCompatibleEvent`
  (`apps/webhook_listeners/lib/Db/WebhookListenerMapper.php:89,131`). The card
  events do not, so no webhook fires.
- `contactsinteraction`, `federation`, `files_sharing`, etc.: no references to
  the card event classes.

---

## 5. Is any of it async / queued?

No. Everything in §§1–3 runs synchronously inside the HTTP request (and inside
the card-write transaction, §0).

Only the *tail* is deferred:
- activity **email** is queued (`oc_activity_mq`) and delivered by the Activity
  app's background job;
- activity **push** is handed to the notifications app (which may itself push
  through Redis/`notify_push`).

Birthday calendar generation as a whole has background jobs
(`GenerateBirthdayCalendarBackgroundJob`, `RegisterRegenerateBirthdayCalendars`,
`apps/dav/lib/BackgroundJob/`), but those are the full-sync/migration path
(`BirthdayService::syncUser()`, `:272-282`) — **not** the per-card-change path.
Per-card-change birthday work is always synchronous.

Reminder delivery is asynchronous (`EventReminderJob`), but the reminder *index
row* is written synchronously by the listener.

---

## 6. Can a Rust sidecar reproduce each effect from DB + Redis + appdata?

| effect | verdict | what it must do |
|---|---|---|
| Activity stream (`oc_activity`) | **yes, with caveats** | INSERT one row per recipient with the exact column set/JSON; reproduce `json_encode` escaping; resolve recipients from `oc_addressbooks`+`oc_dav_shares`+`oc_group_user`; parse vCard for UID/FN |
| Activity mail queue / push | **yes, with caveats** (mail) / **effectively no** (push) | `oc_activity_mq` is a plain INSERT + settings lookup; push needs the notifications app's `oc_notifications` + Redis push channel |
| Birthday calendar objects | **effectively no** (without PHP) | would have to reimplement Sabre VObject 4.0 conversion + byte-exact ICS + denormalized columns + props + changes + synctoken |
| Birthday activity + reminders | **effectively no** | reminder rows come from Sabre recurrence expansion (`EventIterator`); activity cascade is another fan-out |
| Photo cache | **yes, with caveats** | `md5("<abid> <uri>")` folder under `appdata_<instanceid>/dav-photocache/`, deleted through filecache (+ object store) |
| Redis `cloud_id_` invalidation | **yes** | `DEL cloud_id_<cloudId>` for each `CLOUD;…` line of the updated card |

### 6.1 Activity — reproducible, with exactness caveats

- The sidecar already has the card row, the address book and shares available;
  it would need the vCard parse for `UID`/`FN` (already done in `src/vcard.rs`).
- The `subjectparams` value must match PHP `json_encode` defaults: `/` escaped
  as `\/`, non-ASCII escaped as `\uXXXX`, and the exact key insertion order
  (`actor`, `addressbook{id,uri,name}`, `card{id,name}`).
- Settings are simple lookups: `oc_preferences` (`appid='activity'`) and
  `oc_appconfig` (`appid='activity'`); the stream row is written
  unconditionally, so no setting is needed for the DB row itself.
- `activity_log_exclude_users` (system config in `config.php`) gates the write.
- Caveat: the Activity app owns the schema; if it is not installed the table
  does not exist and a Rust writer must detect that. **The Activity app is not
  in this checkout**, so the exact deployed schema/settings must be confirmed
  against the running instance.
- Push notifications: not reproducible without the notifications app
  (`oc_notifications` row + `notify_push`/Redis). Mail is reproducible
  (`oc_activity_mq` INSERT) but the actual send is a PHP background job.

### 6.2 Birthday — effectively no, without PHP

Reproducing it from the DB alone requires reimplementing:
- vCard 4.0 conversion and the `X-APPLE-OMIT-YEAR`/1604 year handling;
- the exact serialized VCalendar bytes (`PRODID`, `DTSTART`/`DTEND;VALUE=DATE`,
  `RRULE`, `X-NEXTCLOUD-BC-*`, `VALARM`), because
  `oc_calendarobjects.etag = md5(serialized bytes)` and clients cache on it;
- denormalized columns (`componenttype`, `firstoccurence`, `lastoccurence`,
  `classification`, `uid`) and the `oc_calendarobjects_props` index;
- `oc_calendarchanges` + `oc_calendars.synctoken` semantics (pre-increment
  token);
- **the reminder index**: `ReminderService` expands recurrences with Sabre's
  `EventIterator` (a yearly RRULE with the Feb-29 special case), which is a
  non-trivial date library to replicate;
- the cascaded `oc_activity` rows for the birthday event itself.

The pragmatic answer is that the Rust sidecar should keep handing writes to
PHP (as it does today: `501` → nginx → PHP) and, if it ever takes over writes,
should either (a) invoke a small PHP entry point for the birthday part, or
(b) accept losing birthday sync.

### 6.3 Photo cache — reproducible, with storage caveats

- Deterministic: `md5("<addressBookId> <cardUri>")` folder under
  `appdata_<instanceid>/dav-photocache/`.
- Must delete through the filesystem abstraction: remove the `oc_filecache`
  rows **and** the bytes. On local storage that is the datadirectory path; on
  object store the bytes are `urn:oid:<fileid>` objects, so the fileids must be
  read from `oc_filecache` first.
- Deleting only the physical files leaves stale filecache entries; deleting
  only the filecache rows leaves orphan bytes (leak) and risks
  `newFile()` conflicts. Either way `?photo` currently falls back to PHP, so
  the safest implementation is an async call that lets PHP do it.

### 6.4 What can safely be deferred until after the HTTP response

- **Activity stream insert / mail queue / Redis `cloud_id_` DEL**: yes. The
  client never reads them in the PUT response. Deferring also removes them from
  the write transaction.
- **Photo-cache delete**: yes. Worst case a stale photo is served for a moment.
- **Birthday update**: yes in principle (it is a different collection and
  eventual consistency is acceptable), and deferring would *improve* robustness
  because today a birthday failure aborts the card write (§0). But the deferred
  worker would still need the VObject/reminder logic.
- **Reminder index**: must exist before `EventReminderJob` runs (cron), so
  ordering only has to be "before the next cron tick", which is easy for an
  async worker.

### 6.5 Things that cannot be reconstructed from DB + Redis + appdata alone

- Sabre VObject serialization for the birthday VEVENT (byte-exact ICS, since
  the ETag is `md5` of it) and recurrence expansion for reminders.
- The Activity app's consumer fan-out (mail/notification) and the notifications
  app's `oc_notifications` + push channel.
- Anything that requires the current PHP **session user** as the activity
  author (`triggerCardActivity` uses `IUserSession`; the sidecar knows the
  authenticated uid from its auth layer, so this one is actually derivable).
- Group-share expansion for recipients uses `IGroupManager`, but that is just
  `oc_group_user`, so it is derivable.

---

## 7. The single most important unknown

**What the deployed Activity app actually is.** It is not in this checkout
(`apps/` has no `activity`), yet it owns `oc_activity`/`oc_activity_mq`, decides
the settings keys, and performs the mail/push fan-out. The evidence in §1 for
its SQL comes from `nextcloud/activity` master, which may not match the
production instance's version. A Rust writer that emits `oc_activity` rows
directly is coupling to another app's private schema, and if that app is absent
or a different version, the "reproduced" activity is either a missing table or a
subtly different row. This must be confirmed against the running instance
(installed app version + actual table definition) before any activity write is
implemented outside PHP.
