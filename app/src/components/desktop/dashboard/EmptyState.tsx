import { FilePlus2, Sparkles, Upload } from 'lucide-react';
import { PremiumButton, PremiumSurface } from '../../ui/PremiumPrimitives';

interface EmptyStateProps {
    onUpload: () => void;
}

export function EmptyState({ onUpload }: EmptyStateProps) {
    return (
        <div className="tr-empty-state flex flex-1 items-center justify-center px-6 py-12">
            <PremiumSurface tone="soft" className="tr-empty-state__card">
                <div className="tr-empty-state__visual" aria-hidden="true">
                    <span className="tr-empty-state__orb tr-empty-state__orb--one" />
                    <span className="tr-empty-state__orb tr-empty-state__orb--two" />
                    <div className="tr-empty-state__icon">
                        <FilePlus2 />
                    </div>
                </div>

                <div className="tr-empty-state__copy">
                    <div className="tr-empty-state__eyebrow">
                        <Sparkles />
                        Ready when you are
                    </div>
                    <h3>Nothing here yet</h3>
                    <p>
                        Drop files into this space or add them from your device.
                    </p>
                </div>

                <PremiumButton
                    variant="primary"
                    size="md"
                    icon={<Upload />}
                    onClick={onUpload}
                >
                    Add files
                </PremiumButton>
            </PremiumSurface>
        </div>
    );
}
