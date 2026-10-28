import type { KeyboardEvent } from 'react';

/** Keep keyboard Tab navigation inside a currently visible modal dialog. */
export function containDialogTab(
    event: KeyboardEvent<HTMLElement>,
    dialog: HTMLElement | null,
    activeElement: Element | null,
): void {
    if (event.key !== 'Tab' || !dialog) return;
    const focusable = Array.from(dialog.querySelectorAll<HTMLElement>(
        'button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])',
    )).filter(element => element.getClientRects().length > 0);
    if (focusable.length === 0) {
        event.preventDefault();
        dialog.focus();
        return;
    }
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && (activeElement === first || !dialog.contains(activeElement))) {
        event.preventDefault();
        last.focus();
    } else if (!event.shiftKey && (activeElement === last || !dialog.contains(activeElement))) {
        event.preventDefault();
        first.focus();
    }
}
