<?php

declare(strict_types=1);

/**
 * SPDX-FileCopyrightText: 2026 nextcloud-dav contributors
 * SPDX-License-Identifier: AGPL-3.0-or-later
 */

namespace OCA\NextcloudDav\Repair;

use OCA\NextcloudDav\AppInfo\Application;
use OCP\DB\QueryBuilder\IQueryBuilder;
use OCP\IDBConnection;
use OCP\Migration\IOutput;
use OCP\Migration\IRepairStep;
use Override;

/**
 * Down-migration / uninstall step: drops the sidecar outbox table.
 *
 * Registered under `<repair-steps><uninstall>` in appinfo/info.xml, so it runs
 * when the app is removed (`occ app:remove nextcloud_dav`). The table is
 * addressed through the configured database prefix, never a hardcoded `oc_`.
 *
 * Any rows still present are reported before the drop: removing the app means
 * removing the dispatcher, so undelivered events cannot be delivered any more.
 */
class DropOutbox implements IRepairStep {
	public function __construct(
		private IDBConnection $db,
	) {
	}

	#[\Override]
	public function getName(): string {
		return 'Drop the nextcloud-dav event outbox';
	}

	#[\Override]
	public function run(IOutput $output): void {
		if (!$this->db->tableExists(Application::OUTBOX_TABLE)) {
			$output->info(Application::OUTBOX_TABLE . ' does not exist, nothing to drop');
			return;
		}

		$query = $this->db->getQueryBuilder();
		$pending = (int)$query->select($query->func()->count('seq'))
			->from(Application::OUTBOX_TABLE)
			->where($query->expr()->in('state', $query->createNamedParameter([0, 1], IQueryBuilder::PARAM_INT_ARRAY)))
			->executeQuery()
			->fetchOne();

		$this->db->dropTable(Application::OUTBOX_TABLE);
		$output->info(sprintf('Dropped %s (%d undelivered rows discarded)', Application::OUTBOX_TABLE, $pending));
	}
}