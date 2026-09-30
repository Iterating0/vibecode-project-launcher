import { useEffect, useRef, type ReactNode } from 'react';

interface DialogProps {
  open: boolean;
  title: string;
  description?: string;
  onClose: () => void;
  closeDisabled?: boolean;
  children: ReactNode;
  labelledBy?: string;
}

export function Dialog({
  open,
  title,
  description,
  onClose,
  closeDisabled = false,
  children,
  labelledBy,
}: DialogProps) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const titleId = labelledBy ?? `dialog-title-${title.toLowerCase().replace(/[^a-z0-9]+/g, '-')}`;
  const descriptionId = description ? `${titleId}-description` : undefined;

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) {
      return;
    }

    if (open && !dialog.open) {
      try {
        dialog.showModal();
      } catch {
        dialog.setAttribute('open', '');
      }
    }

    if (!open && dialog.open) {
      dialog.close();
    }
  }, [open]);

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog || !open) {
      return;
    }

    const focusFirstControl = () => {
      const control =
        dialog.querySelector<HTMLElement>('[data-dialog-autofocus]') ??
        dialog.querySelector<HTMLElement>('input, textarea, select, button');
      control?.focus();
    };
    const frame = window.requestAnimationFrame(focusFirstControl);
    return () => {
      window.cancelAnimationFrame(frame);
    };
  }, [open]);

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog || !open) {
      return;
    }

    const handleCancel = (event: Event) => {
      event.preventDefault();
      if (!closeDisabled) {
        onClose();
      }
    };

    dialog.addEventListener('cancel', handleCancel);
    return () => dialog.removeEventListener('cancel', handleCancel);
  }, [closeDisabled, open, onClose]);

  return (
    <dialog
      ref={dialogRef}
      className="dialog-shell"
      aria-labelledby={titleId}
      aria-describedby={descriptionId}
      aria-busy={closeDisabled || undefined}
      onMouseDown={(event) => {
        if (!closeDisabled && event.target === event.currentTarget) {
          onClose();
        }
      }}
    >
      <div className="dialog-panel">
        <div className="dialog-heading">
          <div>
            <p className="eyebrow">LOCAL PROJECTS</p>
            <h2 id={titleId}>{title}</h2>
            {description ? (
              <p id={descriptionId} className="dialog-description">
                {description}
              </p>
            ) : null}
          </div>
          <button
            className="icon-button dialog-close"
            type="button"
            onClick={onClose}
            disabled={closeDisabled}
            aria-label={closeDisabled ? '保存中，暂不可关闭' : '关闭'}
          >
            ×
          </button>
        </div>
        {children}
      </div>
    </dialog>
  );
}
