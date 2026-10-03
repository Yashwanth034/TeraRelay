interface TeraRelayBrandProps {
    size?: 'sm' | 'md' | 'lg';
    showName?: boolean;
    subtitle?: string;
    className?: string;
}

const sizes = {
    sm: { mark: 'w-8 h-8', name: 'text-sm' },
    md: { mark: 'w-11 h-11', name: 'text-lg' },
    lg: { mark: 'w-16 h-16', name: 'text-xl' },
};

export function TeraRelayBrand({
    size = 'md',
    showName = true,
    subtitle,
    className = '',
}: TeraRelayBrandProps) {
    const style = sizes[size];

    return (
        <div className={`flex items-center gap-3 ${className}`}>
            <img
                src="/logo.svg"
                alt="TeraRelay"
                className={`${style.mark} shrink-0 block`}
                draggable={false}
            />
            {showName && (
                <div className="min-w-0">
                    <div className={`${style.name} font-semibold tracking-tight text-telegram-text leading-tight`}>
                        TeraRelay
                    </div>
                    {subtitle && (
                        <div className="mt-0.5 text-xs text-telegram-subtext leading-tight">
                            {subtitle}
                        </div>
                    )}
                </div>
            )}
        </div>
    );
}
