import { useEffect, type RefObject } from 'react';

/** Decorate native scrolling without rerendering React or handling wheel input. */
export function attachScrollAffordance(viewport: HTMLElement): () => void {
    let frame = 0;
    let hideTimer = 0;
    const paint = () => {
        frame = 0;
        const max = Math.max(0, viewport.scrollHeight - viewport.clientHeight);
        const top = Math.max(0, Math.min(max, viewport.scrollTop));
        viewport.style.setProperty('--tr-scroll-top', `${Math.min(16, top)}px`);
        viewport.style.setProperty('--tr-scroll-bottom', `${Math.min(16, max - top)}px`);
    };
    const schedule = () => {
        if (!frame) frame = requestAnimationFrame(paint);
    };
    const onScroll = () => {
        schedule();
        viewport.dataset.scrollActive = 'true';
        window.clearTimeout(hideTimer);
        hideTimer = window.setTimeout(() => {
            delete viewport.dataset.scrollActive;
            hideTimer = 0;
        }, 900);
    };
    paint();
    viewport.addEventListener('scroll', onScroll, { passive: true });
    const observer = new ResizeObserver(schedule);
    observer.observe(viewport);
    if (viewport.firstElementChild) observer.observe(viewport.firstElementChild);
    if (viewport.lastElementChild && viewport.lastElementChild !== viewport.firstElementChild) {
        observer.observe(viewport.lastElementChild);
    }
    return () => {
        viewport.removeEventListener('scroll', onScroll);
        observer.disconnect();
        cancelAnimationFrame(frame);
        window.clearTimeout(hideTimer);
        delete viewport.dataset.scrollActive;
        viewport.style.removeProperty('--tr-scroll-top');
        viewport.style.removeProperty('--tr-scroll-bottom');
    };
}

export function useScrollAffordance(
    ref: RefObject<HTMLElement | null>,
    ready = true,
    contentVersion: string | number = 0,
): void {
    useEffect(() => {
        if (ready && ref.current) return attachScrollAffordance(ref.current);
    }, [ref, ready, contentVersion]);
}
