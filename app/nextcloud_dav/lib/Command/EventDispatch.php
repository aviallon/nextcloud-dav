<?php

declare(strict_types=1);

/**
 * SPDX-FileCopyrightText: 2026 nextcloud-dav contributors
 * SPDX-License-Identifier: AGPL-3.0-or-later
 */

namespace OCA\NextcloudDav\Command;

use OCA\DAV\CardDAV\CardDavBackend;
use OCA\DAV\Events\CardCreatedEvent;
use OCA\DAV\Events\CardDeletedEvent;
use OCA\DAV\Events\CardUpdatedEvent;
use OCA\NextcloudDav\AppInfo\Application;
use OCA\NextcloudDav\Outbox\OutboxRepository;
use OCA\NextcloudDav\Outbox\PostgresListener;
use OC\Federation\CloudIdManager;
use OCP\App\IAppManager;
use OCP\DB\QueryBuilder\ConflictResolutionMode;
use OCP\DB\QueryBuilder\IQueryBuilder;
use OCP\EventDispatcher\IEventDispatcher;
use OCP\Files\ISetupManager;
use OCP\IConfig;
use OCP\IDBConnection;
use OCP\ITempManager;
use OCP\Server;
use Override;
use Psr\Log\LoggerInterface;
use Symfony\Component\Console\Attribute\AsCommand;
use Symfony\Component\Console\Command\Command;
use Symfony\Component\Console\Input\InputInterface;
use Symfony\Component\Console\Input\InputOption;
use Symfony\Component\Console\Output\OutputInterface;
use Throwable;

/**
 * Resident worker draining <prefix>dav_event_outbox.
 *
 * Bootstrapping (OC::boot/initForRequest) happens once per process, before the
 * loop; the `dav` app is loaded once so its Card*Event listeners are
 * registered; `OC\Federation\CloudIdManager` is instantiated once so its
 * CardUpdatedEvent listener (the Redis `cloud_id_` DEL) is registered.
 *
 * Delivery is at-least-once at the transport level and exactly-once for every
 * database effect: a claimed batch is dispatched and its rows are marked done
 * in the SAME transaction, so a crash replays the whole batch. The effects
 * touched here (activity rows, birthday calendar, photo cache, Redis DEL) are
 * either in that transaction or idempotent.
 */
#[AsCommand(
	name: 'dav:event-dispatch',
	description: 'Drain the nextcloud-dav sidecar event outbox and dispatch CardDAV events',
)]
class EventDispatch extends Command {
	private const EVENT_CREATE = 1;
	private const EVENT_UPDATE = 2;
	private const EVENT_DELETE = 3;

	private const DEFAULT_BATCH = 128;
	private const DEFAULT_IDLE_POLL_MS = 250;
	private const DEFAULT_STOP_AFTER = 3600;
	private const DEFAULT_MAX_REQUESTS = 10000;
	private const DEFAULT_MAX_ATTEMPTS = 8;
	private const DEFAULT_CLAIM_TIMEOUT_S = 300;
	private const DEFAULT_BACKOFF_MS = [100, 500, 2000, 10000, 30000, 60000, 120000];
	// Must match nextcloud-dav/src/config.rs DEFAULT_EVENT_NOTIFY_CHANNEL and the
	// recon default; the channel is NOT derived from the table prefix (the Rust
	// producer's default is literal too). Both sides may override it in config.
	private const DEFAULT_NOTIFY_CHANNEL = 'oc_dav_event_outbox';
	private const STATUS_INTERVAL_S = 10;

	private bool $stopping = false;

	public function __construct(
		private readonly IConfig $config,
		private readonly IDBConnection $db,
		private readonly IAppManager $appManager,
		private readonly ISetupManager $setupManager,
		private readonly ITempManager $tempManager,
		private readonly LoggerInterface $logger,
	) {
		parent::__construct();
	}

	#[\Override]
	protected function configure(): void {
		$this
			->addOption('batch', null, InputOption::VALUE_OPTIONAL, 'Maximum number of outbox rows per claim (built-in default 128, config nextcloud_dav.event_dispatch.batch_size)')
			->addOption('idle-poll-ms', null, InputOption::VALUE_OPTIONAL, 'Safety-net poll interval in milliseconds (built-in default 250, config nextcloud_dav.event_dispatch.idle_poll_ms)')
			->addOption('stop-after', null, InputOption::VALUE_OPTIONAL, 'Exit after this many seconds, bounded lifetime for the supervisor (built-in default 3600, config nextcloud_dav.event_dispatch.php_worker.stop_after_s)')
			->addOption('max-requests', null, InputOption::VALUE_OPTIONAL, 'Exit after dispatching this many rows (built-in default 10000, config nextcloud_dav.event_dispatch.php_worker.max_requests)')
			->addOption('once', null, InputOption::VALUE_NONE, 'Claim and process a single batch, then exit');
	}

	#[\Override]
	protected function execute(InputInterface $input, OutputInterface $output): int {
		$settings = $this->readSettings($input);

		if ((string)$this->config->getSystemValue('dbtype', '') !== 'pgsql') {
			$output->writeln('<error>nextcloud_dav requires PostgreSQL: the sidecar outbox uses bigserial, bytea and FOR UPDATE SKIP LOCKED.</error>');
			return self::FAILURE;
		}

		// Boot Nextcloud once per process. Through `occ` this has already
		// happened in lib/base.php; only bootstrap when we are embedded.
		if (!isset(\OC::$server)) {
			\OC::boot();
			\OC::initForRequest();
		}

		if (!$this->loadDav($output)) {
			return self::FAILURE;
		}

		/** @var CardDavBackend $cardDavBackend */
		$cardDavBackend = Server::get(CardDavBackend::class);
		$dispatcher = Server::get(IEventDispatcher::class);

		// Registers CardUpdatedEvent -> CloudIdManager::handleCardEvent (the
		// Redis cloud_id_ DEL). Without this the listener never runs.
		Server::get(CloudIdManager::class);

		$listener = new PostgresListener($settings['notify_channel'], $this->logger);
		if ($listener->connect()) {
			$output->writeln(sprintf(
				'<info>LISTEN %s active; wake-on-commit with a %d ms safety-net poll.</info>',
				$settings['notify_channel'],
				$settings['idle_poll_ms'],
			));
		} else {
			$output->writeln(sprintf(
				'<comment>LISTEN unavailable (%s); falling back to plain polling every %d ms.</comment>',
				$listener->getUnavailableReason(),
				$settings['idle_poll_ms'],
			));
		}

		$this->installSignalHandlers();

		if (!$settings['php_generic_dispatch']) {
			$output->writeln('<comment>nextcloud_dav.event_dispatch.php_generic_dispatch is false, but selective PHP dispatch is not implemented; generic dispatchTyped() is used. Keep it true until the native phase lands.</comment>');
			$this->logger->warning('nextcloud_dav: php_generic_dispatch is false but selective dispatch is not implemented; using generic dispatch');
		}

		$workerId = $this->workerId();
		$startedAt = time();
		$processed = 0;
		$lastStatusAt = 0;

		while (true) {
			$this->dispatchSignals();
			if ($this->stopping) {
				$output->writeln('<info>SIGTERM/SIGINT received; stopping after the current batch.</info>');
				break;
			}
			if (!$settings['once'] && $settings['stop_after'] > 0 && time() >= $startedAt + $settings['stop_after']) {
				$output->writeln('<info>stop-after reached; exiting for the supervisor to restart.</info>', OutputInterface::VERBOSITY_VERBOSE);
				break;
			}
			if (!$settings['once'] && $settings['max_requests'] > 0 && $processed >= $settings['max_requests']) {
				$output->writeln('<info>max-requests reached; exiting for the supervisor to restart.</info>', OutputInterface::VERBOSITY_VERBOSE);
				break;
			}

			$now = time();
			$rows = $this->claimBatch($settings['batch'], $workerId, $now, $now - $settings['claim_timeout_s']);
			if ($rows === []) {
				if ($settings['once']) {
					$output->writeln('No outbox rows to dispatch.', OutputInterface::VERBOSITY_VERBOSE);
					break;
				}
				if (time() - $lastStatusAt >= self::STATUS_INTERVAL_S) {
					$lastStatusAt = time();
					$this->printStatus($output, $now, $processed);
				}
				$listener->wait($settings['idle_poll_ms']);
				continue;
			}

			$batchSize = count($rows);
			try {
				$this->db->beginTransaction();
				foreach ($rows as $row) {
					$this->dispatchRow($row, $cardDavBackend, $dispatcher, $output);
				}
				$this->markDone(array_map(static fn (array $row): int => (int)$row['seq'], $rows), time());
				$this->db->commit();
				$processed += $batchSize;
			} catch (Throwable $e) {
				$this->db->rollBack();
				$this->scheduleRetry($rows, $e, $settings['max_attempts'], $settings['backoff_ms']);
				$this->logger->error('nextcloud_dav: dispatch batch failed, scheduled retry', [
					'app' => 'nextcloud_dav',
					'rows' => $batchSize,
					'exception' => $e,
				]);
				$output->writeln(sprintf('<error>Batch of %d failed: %s</error>', $batchSize, $e->getMessage()));
			}

			// Per-batch hygiene, cf. core/Command/Background/JobWorker.php.
			$this->setupManager->tearDown();
			$this->tempManager->clean();
			gc_collect_cycles();

			$lastStatusAt = time();
			$this->printStatus($output, time(), $processed);
		}

		$listener->close();
		$this->printStatus($output, time(), $processed);

		return self::SUCCESS;
	}

	/**
	 * @return list<array<string, mixed>>
	 */
	private function claimBatch(int $batch, string $workerId, int $now, int $staleBefore): array {
		$this->db->beginTransaction();
		try {
			$query = $this->db->getQueryBuilder();
			$query->select('seq', 'created_at', 'event_type', 'addressbookid', 'card_uri', 'card_row', 'card_data', 'effects', 'attempts')
				->from(OutboxRepository::TABLE)
				->where($query->expr()->orX(
					$query->expr()->andX(
						$query->expr()->eq('state', $query->createNamedParameter(OutboxRepository::STATE_PENDING, IQueryBuilder::PARAM_INT)),
						$query->expr()->lte('next_attempt_at', $query->createNamedParameter($now, IQueryBuilder::PARAM_INT)),
					),
					$query->expr()->andX(
						$query->expr()->eq('state', $query->createNamedParameter(OutboxRepository::STATE_CLAIMED, IQueryBuilder::PARAM_INT)),
						$query->expr()->isNotNull('reserved_at'),
						$query->expr()->lt('reserved_at', $query->createNamedParameter($staleBefore, IQueryBuilder::PARAM_INT)),
					),
				))
				->orderBy('seq', 'ASC')
				->setMaxResults($batch)
				->forUpdate(ConflictResolutionMode::SkipLocked);

			$rows = $query->executeQuery()->fetchAllAssociative();

			if ($rows !== []) {
				$seqs = array_map(static fn (array $row): int => (int)$row['seq'], $rows);
				$update = $this->db->getQueryBuilder();
				$update->update(OutboxRepository::TABLE)
					->set('state', $update->createNamedParameter(OutboxRepository::STATE_CLAIMED, IQueryBuilder::PARAM_INT))
					->set('reserved_by', $update->createNamedParameter($workerId))
					->set('reserved_at', $update->createNamedParameter($now, IQueryBuilder::PARAM_INT))
					->where($update->expr()->in('seq', $update->createNamedParameter($seqs, IQueryBuilder::PARAM_INT_ARRAY)));
				$update->executeStatement();
			}

			$this->db->commit();
			return $rows;
		} catch (Throwable $e) {
			$this->db->rollBack();
			throw $e;
		}
	}

	/**
	 * @param array<string, mixed> $row
	 */
	private function dispatchRow(array $row, CardDavBackend $cardDavBackend, IEventDispatcher $dispatcher, OutputInterface $output): void {
		$effects = json_decode((string)$row['effects'], true, 512, JSON_THROW_ON_ERROR);
		$phpEffects = is_array($effects['php'] ?? null) ? $effects['php'] : [];
		if ($phpEffects === []) {
			// No PHP-owned effect on this row (phase 2+: claimed by the Rust
			// backend). Mark it done without dispatching.
			return;
		}

		$addressBookId = (int)$row['addressbookid'];
		$cardRow = json_decode((string)$row['card_row'], true, 512, JSON_THROW_ON_ERROR);
		if (!is_array($cardRow)) {
			throw new \UnexpectedValueException('card_row is not a JSON object');
		}

		$cardData = $row['card_data'];
		if (is_resource($cardData)) {
			$cardData = stream_get_contents($cardData);
		}
		$cardData = (string)$cardData;

		$cardRow['addressbookid'] = $addressBookId;
		$cardRow['carddata'] = $cardData;

		$addressBookData = $cardDavBackend->getAddressBookById($addressBookId) ?? [];
		$shares = $cardDavBackend->getShares($addressBookId);

		$event = match ((int)$row['event_type']) {
			self::EVENT_CREATE => new CardCreatedEvent($addressBookId, $addressBookData, $shares, $cardRow),
			self::EVENT_UPDATE => new CardUpdatedEvent($addressBookId, $addressBookData, $shares, $cardRow),
			self::EVENT_DELETE => new CardDeletedEvent($addressBookId, $addressBookData, $shares, $cardRow),
			default => throw new \UnexpectedValueException('unknown event_type ' . $row['event_type']),
		};

		$dispatcher->dispatchTyped($event);

		$output->writeln(sprintf(
			'Dispatched %s for addressbook %d card %s',
			$event::class,
			$addressBookId,
			(string)$row['card_uri'],
		), OutputInterface::VERBOSITY_VERY_VERBOSE);
	}

	/**
	 * @param list<int> $seqs
	 */
	private function markDone(array $seqs, int $now): void {
		if ($seqs === []) {
			return;
		}
		$update = $this->db->getQueryBuilder();
		$update->update(OutboxRepository::TABLE)
			->set('state', $update->createNamedParameter(OutboxRepository::STATE_DONE, IQueryBuilder::PARAM_INT))
			->set('processed_at', $update->createNamedParameter($now, IQueryBuilder::PARAM_INT))
			->set('reserved_by', $update->createNamedParameter(null, IQueryBuilder::PARAM_NULL))
			->set('reserved_at', $update->createNamedParameter(null, IQueryBuilder::PARAM_NULL))
			->where($update->expr()->in('seq', $update->createNamedParameter($seqs, IQueryBuilder::PARAM_INT_ARRAY)));
		$update->executeStatement();
	}

	/**
	 * @param list<array<string, mixed>> $rows
	 * @param list<int> $backoffMs
	 */
	private function scheduleRetry(array $rows, Throwable $error, int $maxAttempts, array $backoffMs): void {
		$message = mb_substr($error->getMessage(), 0, 2000);
		$now = time();

		$this->db->beginTransaction();
		try {
			foreach ($rows as $row) {
				$attempts = (int)$row['attempts'] + 1;
				$update = $this->db->getQueryBuilder();
				$update->update(OutboxRepository::TABLE)
					->set('attempts', $update->createNamedParameter($attempts, IQueryBuilder::PARAM_INT))
					->set('last_error', $update->createNamedParameter($message))
					->set('reserved_by', $update->createNamedParameter(null, IQueryBuilder::PARAM_NULL))
					->set('reserved_at', $update->createNamedParameter(null, IQueryBuilder::PARAM_NULL));

				if ($attempts >= $maxAttempts) {
					$update->set('state', $update->createNamedParameter(OutboxRepository::STATE_DEAD, IQueryBuilder::PARAM_INT))
						->set('processed_at', $update->createNamedParameter($now, IQueryBuilder::PARAM_INT));
				} else {
					$delayMs = $backoffMs[min($attempts - 1, count($backoffMs) - 1)] ?? 0;
					$update->set('state', $update->createNamedParameter(OutboxRepository::STATE_PENDING, IQueryBuilder::PARAM_INT))
						->set('next_attempt_at', $update->createNamedParameter($now + (int)ceil($delayMs / 1000), IQueryBuilder::PARAM_INT));
				}

				$update->where($update->expr()->eq('seq', $update->createNamedParameter((int)$row['seq'], IQueryBuilder::PARAM_INT)));
				$update->executeStatement();
			}
			$this->db->commit();
		} catch (Throwable $nested) {
			$this->db->rollBack();
			$this->logger->error('nextcloud_dav: failed to schedule retry for a batch', [
				'app' => 'nextcloud_dav',
				'exception' => $nested,
			]);
		}
	}

	private function printStatus(OutputInterface $output, int $now, int $processedThisRun): void {
		$counts = $this->repository()->counts();
		$oldestAge = $counts['oldest_pending_at'] !== null ? max(0, $now - $counts['oldest_pending_at']) : 0;
		$output->writeln(sprintf(
			'pending=%d in_flight=%d dead=%d oldest_pending=%ds processed_this_run=%d',
			$counts['pending'],
			$counts['claimed'],
			$counts['dead'],
			$oldestAge,
			$processedThisRun,
		));
	}

	private function repository(): OutboxRepository {
		return new OutboxRepository($this->db);
	}

	private function loadDav(OutputInterface $output): bool {
		if (!$this->appManager->isEnabledForUser('dav') && !$this->appManager->isInstalled('dav')) {
			$output->writeln('<error>The `dav` app is not installed/enabled; nextcloud_dav cannot dispatch CardDAV events.</error>');
			return false;
		}

		try {
			$this->appManager->loadApp('dav');
		} catch (Throwable $e) {
			$output->writeln('<error>Could not load the `dav` app: ' . $e->getMessage() . '</error>');
			return false;
		}

		if (!class_exists(CardCreatedEvent::class)) {
			$output->writeln('<error>OCA\DAV\Events\CardCreatedEvent is missing; the `dav` app is too old.</error>');
			return false;
		}

		return true;
	}

	private function installSignalHandlers(): void {
		if (!function_exists('pcntl_signal') || !defined('SIGTERM') || !defined('SIGINT')) {
			return;
		}

		$handler = function (int $signal): void {
			$this->stopping = true;
		};
		pcntl_signal(SIGTERM, $handler);
		pcntl_signal(SIGINT, $handler);
	}

	private function dispatchSignals(): void {
		if (function_exists('pcntl_signal_dispatch')) {
			pcntl_signal_dispatch();
		}
	}

	private function workerId(): string {
		$host = function_exists('gethostname') ? (string)gethostname() : 'worker';
		$id = $host . ':' . getmypid();
		return mb_substr($id, 0, 64);
	}

	/**
	 * Merge config.php defaults with the CLI options.
	 *
	 * @return array{
	 *   batch: int,
	 *   idle_poll_ms: int,
	 *   stop_after: int,
	 *   max_requests: int,
	 *   once: bool,
	 *   max_attempts: int,
	 *   backoff_ms: list<int>,
	 *   claim_timeout_s: int,
	 *   notify_channel: string,
	 *   php_generic_dispatch: bool
	 * }
	 */
	private function readSettings(InputInterface $input): array {
		$config = $this->config->getSystemValue(Application::CONFIG_KEY, []);
		$config = is_array($config) ? $config : [];
		$dispatch = $config[Application::CONFIG_DISPATCH_KEY] ?? [];
		$dispatch = is_array($dispatch) ? $dispatch : [];
		$worker = is_array($dispatch['php_worker'] ?? null) ? $dispatch['php_worker'] : [];

		$backoff = $dispatch['backoff_ms'] ?? self::DEFAULT_BACKOFF_MS;
		if (!is_array($backoff) || $backoff === []) {
			$backoff = self::DEFAULT_BACKOFF_MS;
		}
		$backoff = array_values(array_map(static fn ($value): int => max(0, (int)$value), $backoff));

		$defaultChannel = self::DEFAULT_NOTIFY_CHANNEL;

		return [
			'batch' => max(1, (int)($input->getOption('batch') ?? $dispatch['batch_size'] ?? self::DEFAULT_BATCH)),
			'idle_poll_ms' => max(10, (int)($input->getOption('idle-poll-ms') ?? $dispatch['idle_poll_ms'] ?? self::DEFAULT_IDLE_POLL_MS)),
			'stop_after' => max(0, (int)($input->getOption('stop-after') ?? $worker['stop_after_s'] ?? self::DEFAULT_STOP_AFTER)),
			'max_requests' => max(0, (int)($input->getOption('max-requests') ?? $worker['max_requests'] ?? self::DEFAULT_MAX_REQUESTS)),
			'once' => (bool)$input->getOption('once'),
			'max_attempts' => max(1, (int)($dispatch['max_attempts'] ?? self::DEFAULT_MAX_ATTEMPTS)),
			'backoff_ms' => $backoff,
			'claim_timeout_s' => max(1, (int)($dispatch['claim_timeout_s'] ?? self::DEFAULT_CLAIM_TIMEOUT_S)),
			'notify_channel' => (string)($dispatch['notify_channel'] ?? $defaultChannel),
			'php_generic_dispatch' => (bool)($dispatch['php_generic_dispatch'] ?? true),
		];
	}
}