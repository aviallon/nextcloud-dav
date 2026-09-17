<?php

/**
 * SPDX-FileCopyrightText: 2026 nextcloud-dav contributors
 * SPDX-License-Identifier: AGPL-3.0-or-later
 */

/** @var array{pending: int, claimed: int, dead: int, oldestPendingAge: int} $_ */
?>
<div class="section">
	<h2>Nextcloud DAV event outbox</h2>
	<p class="settings-hint">
		Backlog of the sidecar transaction outbox drained by
		<code>occ dav:event-dispatch</code>.
	</p>
	<table>
		<tbody>
			<tr>
				<td>Pending (state 0)</td>
				<td><?php p((string)$_['pending']); ?></td>
			</tr>
			<tr>
				<td>In flight (state 1)</td>
				<td><?php p((string)$_['claimed']); ?></td>
			</tr>
			<tr>
				<td>Dead letters (state 3)</td>
				<td><?php p((string)$_['dead']); ?></td>
			</tr>
			<tr>
				<td>Oldest pending age</td>
				<td><?php p((string)$_['oldestPendingAge']); ?> s</td>
			</tr>
		</tbody>
	</table>
</div>