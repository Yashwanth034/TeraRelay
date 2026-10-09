import type { ButtonHTMLAttributes, HTMLAttributes, ReactNode } from 'react';

type ButtonVariant = 'primary' | 'secondary' | 'ghost' | 'danger';
type ButtonSize = 'sm' | 'md';

function cx(...classes: Array<string | false | null | undefined>) {
    return classes.filter(Boolean).join(' ');
}

interface PremiumButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
    variant?: ButtonVariant;
    size?: ButtonSize;
    icon?: ReactNode;
}

export function PremiumButton({
    variant = 'secondary',
    size = 'md',
    icon,
    className,
    children,
    ...props
}: PremiumButtonProps) {
    return (
        <button
            className={cx(
                'tr-button',
                `tr-button--${variant}`,
                `tr-button--${size}`,
                className,
            )}
            {...props}
        >
            {icon && <span className="tr-button__icon" aria-hidden="true">{icon}</span>}
            {children && <span className="tr-button__label">{children}</span>}
        </button>
    );
}

interface PremiumIconButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
    label: string;
    active?: boolean;
    tone?: 'default' | 'primary' | 'danger';
}

export function PremiumIconButton({
    label,
    active = false,
    tone = 'default',
    className,
    children,
    ...props
}: PremiumIconButtonProps) {
    return (
        <button
            className={cx(
                'tr-icon-button',
                active && 'tr-icon-button--active',
                tone === 'primary' && 'tr-icon-button--primary',
                tone === 'danger' && 'tr-icon-button--danger',
                className,
            )}
            aria-label={label}
            title={label}
            {...props}
        >
            {children}
        </button>
    );
}

interface PremiumSurfaceProps extends HTMLAttributes<HTMLDivElement> {
    tone?: 'glass' | 'raised' | 'soft';
}

export function PremiumSurface({
    tone = 'glass',
    className,
    children,
    ...props
}: PremiumSurfaceProps) {
    return (
        <div className={cx('tr-surface', `tr-surface--${tone}`, className)} {...props}>
            {children}
        </div>
    );
}

interface PremiumBadgeProps extends HTMLAttributes<HTMLSpanElement> {
    tone?: 'neutral' | 'primary' | 'success' | 'warning' | 'danger';
    dot?: boolean;
}

export function PremiumBadge({
    tone = 'neutral',
    dot = false,
    className,
    children,
    ...props
}: PremiumBadgeProps) {
    return (
        <span className={cx('tr-badge', `tr-badge--${tone}`, className)} {...props}>
            {dot && <span className="tr-badge__dot" aria-hidden="true" />}
            {children}
        </span>
    );
}
