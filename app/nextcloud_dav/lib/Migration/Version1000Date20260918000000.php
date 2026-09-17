<?php

declare(strict_types=1);

/**
 * SPDX-FileCopyrightText: 2026 nextcloud-dav contributors
 * SPDX-License-Identifier: AGPL-3.0-or-later
 */

namespace OCA\NextcloudDav\Migration;

use Closure;
use OCA\NextcloudDav\AppInfo\Application;
use OCP\DB\ISchemaWrapper;
use OCP\DB\Types;
use OCP\Migration\IOutput;
use OCP\Migration\SimpleMigrationStep;
use Override;

/**
 * Creates the sidecar transaction outbox.
 *
 * This schema is a frozen interface: the Rust sidecar writes rows into it in
 * the same transaction as the card write. Do not rename columns or change
 * types without coordinating with `nextcloud-dav/src/`.
 *
 *   event_type: 1 = create, 2 = update, 3 = delete
 *   card_row:   JSON {id,uri,lastmodified,etag,size,uid}
 *   card_data:  the readBlob()-filtered carddata (bytea; no 32k limit)
 *   effects:    JSON {"php":[...],"rust":[...]} ownership map
 */
class Version1000Date20260918000000 extends SimpleMigrationStep {
	#[\Override]
	public function name(): string {
		return 'Create the nextcloud-dav event outbox';
	}

	#[\Override]
	public function description(): string {
		return 'Creates <prefix>dav_event_outbox, the transactional outbox drained by occ dav:event-dispatch.';
	}

	/**
	 * @param Closure():ISchemaWrapper $schemaClosure
	 */
	#[\Override]
	public function changeSchema(IOutput $output, Closure $schemaClosure, array $options): ?ISchemaWrapper {
		/** @var ISchemaWrapper $schema */
		$schema = $schemaClosure();

		if ($schema->hasTable(Application::OUTBOX_TABLE)) {
			return null;
		}

		$table = $schema->createTable(Application::OUTBOX_TABLE);

		// bigserial PRIMARY KEY: Doctrine maps autoincrement bigint to BIGSERIAL on PostgreSQL.
		$table->addColumn('seq', Types::BIGINT, [
			'autoincrement' => true,
			'notnull' => true,
		]);
		$table->addColumn('created_at', Types::BIGINT, [
			'notnull' => true,
		]);
		$table->addColumn('event_type', Types::SMALLINT, [
			'notnull' => true,
		]);
		$table->addColumn('addressbookid', Types::BIGINT, [
			'notnull' => true,
		]);
		$table->addColumn('card_uri', Types::STRING, [
			'notnull' => true,
			'length' => 255,
		]);
		$table->addColumn('card_row', Types::TEXT, [
			'notnull' => true,
		]);
		$table->addColumn('card_data', Types::BLOB, [
			'notnull' => true,
		]);
		$table->addColumn('effects', Types::TEXT, [
			'notnull' => true,
		]);
		$table->addColumn('state', Types::SMALLINT, [
			'notnull' => true,
			'default' => 0,
		]);
		$table->addColumn('attempts', Types::SMALLINT, [
			'notnull' => true,
			'default' => 0,
		]);
		$table->addColumn('next_attempt_at', Types::BIGINT, [
			'notnull' => true,
			'default' => 0,
		]);
		$table->addColumn('reserved_by', Types::STRING, [
			'notnull' => false,
			'length' => 64,
		]);
		$table->addColumn('reserved_at', Types::BIGINT, [
			'notnull' => false,
		]);
		$table->addColumn('processed_at', Types::BIGINT, [
			'notnull' => false,
		]);
		$table->addColumn('last_error', Types::TEXT, [
			'notnull' => false,
		]);

		$table->setPrimaryKey(['seq']);
		// Keep the index name exactly as documented in the frozen interface:
		// <prefix>dav_event_outbox_pending_idx (Doctrine does not prefix index
		// names, and MigrationService always supplies the configured prefix).
		$table->addIndex(
			['state', 'next_attempt_at', 'seq'],
			($options['tablePrefix'] ?? '') . 'dav_event_outbox_pending_idx'
		);

		return $schema;
	}
}