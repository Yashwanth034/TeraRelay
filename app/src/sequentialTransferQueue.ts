/** Pick the next file in original enqueue order, never starting two files together.
 * An active reservation covers preparation, transfer, and durable finalization,
 * not just the time spent sending bytes to Telegram.
 */
export function nextSequentialTransfer<T extends { id: string; status: string }>(
    queue: readonly T[],
    activeIds: ReadonlySet<string>,
): T | undefined {
    if (activeIds.size > 0) return undefined;
    return queue.find(item => item.status === 'pending' && !activeIds.has(item.id));
}
