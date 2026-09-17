<?php

declare(strict_types=1);

/**
 * SPDX-FileCopyrightText: 2026 nextcloud-dav contributors
 * SPDX-License-Identifier: AGPL-3.0-or-later
 */

namespace OCA\NextcloudDav\Outbox;

use OCA\NextcloudDav\AppInfo\Application;
use OCP\DB\QueryBuilder\IQueryBuilder;
use OCP\IDBConnection;

/**
 * Read-only helpers over the sidecar outbox.
 *
 * The write side (claim / mark done / schedule retry) lives in
 * {@see \OCA\NextcloudDav\Command\EventDispatch} because it is tied to the
 * dispatch loop and its transaction boundaries.
 */
final class OutboxRepository {
	public const TABLE = Application::OUTBOX_TABLE;

	public const STATE_PENDING = 0;
	public const STATE_CLAIMED = 1;
	public const STATE_DONE = 2;
	public const STATE_DEAD = 3;

	public function __construct(
		private IDBConnection $db,
	) {
	}

	/**
	 * @return array{pending: int, claimed: int, dead: int, oldest_pending_at: int|null}
	 */
	public function counts(): array {
		$query = $this->db->getQueryBuilder();
		$query->select('state')
			->selectAlias($query->func()->count('seq'), 'c')
			->selectAlias($query->func()->min('created_at'), 'oldest')
			->from(self::TABLE)
			->where($query->expr()->neq('state', $query->createNamedParameter(self::STATE_DONE, IQueryBuilder::PARAM_INT)))
			->groupBy('state');

		$result = [
			'pending' => 0,
			'claimed' => 0,
			'dead' => 0,
			'oldest_pending_at' => null,
		];

		foreach ($query->executeQuery()->fetchAllAssociative() as $row) {
			$count = (int)$row['c'];
			switch ((int)$row['state']) {
				case self::STATE_PENDING:
					$result['pending'] = $count;
					$result['oldest_pending_at'] = $row['oldest'] !== null ? (int)$row['oldest'] : null;
					break;
				case self::STATE_CLAIMED:
					$result['claimed'] = $count;
					break;
				case self::STATE_DEAD:
					$result['dead'] = $count;
					break;
			}
		}

		return $result;
	}

	public function pendingCount(): int {
		return $this->counts()['pending'];
	}

	public function deadCount(): int {
		return $this->counts()['dead'];
	}

	/**
	 * Re-arm rows whose worker died without releasing its claim (state = 1 with
	 * a reservation older than the claim timeout).
	 *
	 * `attempts` is deliberately NOT incremented: a killed worker is an
	 * infrastructure event, not a poison event, and a re-armed row must not
	 * drift towards the dead-letter state just because the pod restarted.
	 *
	 * @return int number of re-armed rows
	 */
	public function reapStaleReservations(int $staleBefore): int {
		$query = $this->db->getQueryBuilder();
		$query->update(self::TABLE)
			->set('state', $query->createNamedParameter(self::STATE_PENDING, IQueryBuilder::PARAM_INT))
			->set('reserved_by', $query->createNamedParameter(null, IQueryBuilder::PARAM_NULL))
			->set('reserved_at', $query->createNamedParameter(null, IQueryBuilder::PARAM_NULL))
			->where($query->expr()->eq('state', $query->createNamedParameter(self::STATE_CLAIMED, IQueryBuilder::PARAM_INT)))
			->andWhere($query->expr()->isNotNull('reserved_at'))
			->andWhere($query->expr()->lt('reserved_at', $query->createNamedParameter($staleBefore, IQueryBuilder::PARAM_INT)));

		return $query->executeStatement();
	}

	/**
	 * @return int number of deleted rows
	 */
	public function deleteDoneBefore(int $cutoff): int {
		$query = $this->db->getQueryBuilder();
		$query->delete(self::TABLE)
			->where($query->expr()->eq('state', $query->createNamedParameter(self::STATE_DONE, IQueryBuilder::PARAM_INT)))
			->andWhere($query->expr()->isNotNull('processed_at'))
			->andWhere($query->expr()->lt('processed_at', $query->createNamedParameter($cutoff, IQueryBuilder::PARAM_INT)));

		return $query->executeStatement();
	}
}