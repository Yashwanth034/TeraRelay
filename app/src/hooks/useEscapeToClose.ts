import { useEffect } from 'react';

export function useEscapeToClose(
    active: boolean,
    onClose: () => void,
    disabled = false,
) {
    useEffect(() => {
        if (!active || disabled) return;

        const handleKeyDown = (event: KeyboardEvent) => {
            if (event.key !== 'Escape') return;
            event.preventDefault();
            event.stopPropagation();
            onClose();
        };

        window.addEventListener('keydown', handleKeyDown, true);
        return () => window.removeEventListener('keydown', handleKeyDown, true);
    }, [active, disabled, onClose]);
}
