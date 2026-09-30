import { useEffect, useRef, useState, type FormEvent } from 'react';

import type { LaunchDetection, LaunchMode, ProjectInput } from '../types';

type PickExecutable = (directory: string) => Promise<string | null>;

interface ProjectFormProps {
  initialValue: ProjectInput;
  submitLabel: string;
  submitting?: boolean;
  error?: string | null;
  onDetect?: (directory: string) => Promise<LaunchDetection>;
  onPickExecutable?: PickExecutable;
  onSubmit: (input: ProjectInput) => void;
  onClose: () => void;
  onRemove?: () => void;
}

export function ProjectForm({
  initialValue,
  submitLabel,
  submitting = false,
  error = null,
  onDetect,
  onPickExecutable,
  onSubmit,
  onClose,
  onRemove,
}: ProjectFormProps) {
  const [value, setValue] = useState<ProjectInput>(() => ({
    ...initialValue,
    launchMode: initialValue.launchMode ?? 'browser',
    executablePath: initialValue.executablePath ?? '',
    arguments: initialValue.arguments ?? '',
  }));
  const [pickingExecutable, setPickingExecutable] = useState(false);
  const [pickerError, setPickerError] = useState<string | null>(null);
  const [detecting, setDetecting] = useState(Boolean(onDetect));
  const [detection, setDetection] = useState<LaunchDetection | null>(null);
  const [detectionMessage, setDetectionMessage] = useState<string | null>(null);
  const [selectedSuggestion, setSelectedSuggestion] = useState('0');
  const detectionEpoch = useRef(0);

  useEffect(() => {
    if (!onDetect) return;
    const epoch = ++detectionEpoch.current;
    setDetecting(true);
    void onDetect(initialValue.path).then((result) => {
      if (detectionEpoch.current !== epoch) return;
      setDetection(result);
      if (result.suggestions[0]) {
        setValue((current) => ({ ...current, ...result.suggestions[0].settings }));
      } else {
        setDetectionMessage('未找到明确的启动入口，请手动选择启动方式并填写配置。');
      }
    }).catch((reason: unknown) => {
      if (detectionEpoch.current !== epoch) return;
      const message = reason instanceof Error ? reason.message : String(reason);
      setDetectionMessage(`自动识别失败：${message}。你仍可手动填写。`);
    }).finally(() => {
      if (detectionEpoch.current === epoch) setDetecting(false);
    });
    return () => { detectionEpoch.current += 1; };
  }, [initialValue.path, onDetect]);

  const skipDetection = () => {
    detectionEpoch.current += 1;
    setDetecting(false);
    setDetectionMessage('已跳过自动识别，请手动填写启动配置。');
  };

  const launchMode: LaunchMode = value.launchMode ?? 'browser';
  const formBusy = submitting || pickingExecutable || detecting;
  const formError = pickerError ?? error;

  const update = <K extends keyof ProjectInput>(key: K, nextValue: ProjectInput[K]) => {
    setValue((current) => ({ ...current, [key]: nextValue }));
  };

  const handleLaunchModeChange = (nextMode: LaunchMode) => {
    setPickerError(null);
    update('launchMode', nextMode);
  };

  const handlePickExecutable = async () => {
    if (!onPickExecutable || formBusy) {
      return;
    }

    setPickerError(null);
    setPickingExecutable(true);
    try {
      const selectedPath = await onPickExecutable(value.path.trim());
      if (selectedPath) {
        update('executablePath', selectedPath);
      }
    } catch (pickError) {
      setPickerError(pickError instanceof Error ? pickError.message : '选择可执行文件失败，请重试。');
    } finally {
      setPickingExecutable(false);
    }
  };

  const handleSubmit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (formBusy) return;
    onSubmit({
      ...value,
      name: value.name.trim(),
      path: value.path.trim(),
      workingDirectory: value.workingDirectory.trim(),
      launchMode,
      executablePath: value.executablePath.trim(),
      arguments: value.arguments.trim(),
      command: value.command.trim(),
      urlTemplate: value.urlTemplate.trim(),
      icon: value.icon?.trim() || null,
    });
  };

  return (
    <form className="project-form" onSubmit={handleSubmit}>
      {onDetect ? (
        <section className="detection-panel" aria-label="启动方式识别">
          {detecting ? (
            <div className="detection-progress">
              <span role="status">正在识别项目启动方式…</span>
              <button type="button" className="button button-quiet" onClick={skipDetection}>手动填写</button>
            </div>
          ) : detection?.suggestions.length ? (
            <>
              <label className="field">
                <span>识别结果 <small>可切换建议并修改下方配置</small></span>
                <select
                  aria-label="识别结果"
                  value={selectedSuggestion}
                  disabled={formBusy || value.path !== initialValue.path}
                  onChange={(event) => {
                    const index = event.target.value;
                    setSelectedSuggestion(index);
                    setPickerError(null);
                    setValue((current) => ({ ...current, ...detection.suggestions[Number(index)].settings }));
                  }}
                >
                  {detection.suggestions.map((suggestion, index) => (
                    <option key={index} value={String(index)}>{suggestion.label}</option>
                  ))}
                </select>
              </label>
              <p>{value.path === initialValue.path
                ? detection.suggestions[Number(selectedSuggestion)].detail
                : '项目路径已更改，请核对下方启动配置；上方建议来自原文件夹。'}</p>
            </>
          ) : <p role="status">{detectionMessage}</p>}
          {detection?.warnings.map((warning) => <p key={warning}>{warning}</p>)}
        </section>
      ) : null}
      {formError ? (
        <div className="form-error" role="alert">
          <strong>{pickerError ? '选择失败' : '保存失败'}</strong>
          <span>{formError}</span>
        </div>
      ) : null}
      <fieldset className="form-grid" disabled={detecting || submitting} aria-label="项目启动配置">
        <label className="field field-span-two">
          <span>项目名称 <small>Project name</small></span>
          <input
            data-dialog-autofocus
            value={value.name}
            onChange={(event) => update('name', event.target.value)}
            placeholder="例如 Dashboard"
            required
            autoComplete="off"
          />
        </label>

        <label className="field field-span-two">
          <span>启动方式 <small>Launch mode</small></span>
          <select
            aria-label="启动方式 Launch mode"
            value={launchMode}
            onChange={(event) => handleLaunchModeChange(event.target.value as LaunchMode)}
            disabled={submitting}
          >
            <option value="browser">浏览器部署 Browser</option>
            <option value="executable">可执行文件部署 Executable</option>
          </select>
        </label>

        <label className="field field-span-two">
          <span>项目路径 <small>Project path</small></span>
          <input
            value={value.path}
            onChange={(event) => update('path', event.target.value)}
            placeholder="D:\\Projects\\dashboard"
            required
            autoComplete="off"
          />
        </label>

        <label className="field field-span-two">
          <span>工作目录 <small>Working directory</small></span>
          <input
            value={value.workingDirectory}
            onChange={(event) => update('workingDirectory', event.target.value)}
            placeholder="默认使用项目路径"
            required
            autoComplete="off"
          />
        </label>

        {launchMode === 'browser' ? (
          <>
            <label className="field field-span-two">
              <span>启动命令 <small>Start command</small></span>
              <input
                value={value.command}
                onChange={(event) => update('command', event.target.value)}
                placeholder="npm run dev -- --port {port}"
                required
                autoComplete="off"
              />
              <small className="field-help">使用 {'{port}'} 让 Launcher 注入实际端口；自动识别的命令也可在此修改。</small>
            </label>

            <label className="field">
              <span>首选端口 <small>Preferred port</small></span>
              <input
                type="number"
                min="1"
                max="65535"
                value={value.preferredPort}
                onChange={(event) => update('preferredPort', Number(event.target.value))}
                required
              />
            </label>

            <label className="field">
              <span>浏览器地址 <small>URL template</small></span>
              <input
                value={value.urlTemplate}
                onChange={(event) => update('urlTemplate', event.target.value)}
                placeholder="http://localhost:{port}"
                required
                autoComplete="off"
              />
            </label>
          </>
        ) : (
          <>
            <div className="field field-span-two">
              <span>可执行文件 <small>Executable</small></span>
              <div className="field-with-button">
                <input
                  aria-label="可执行文件 Executable"
                  value={value.executablePath}
                  onChange={(event) => update('executablePath', event.target.value)}
                  placeholder="选择 .exe、.bat 或 .cmd 文件"
                  required
                  autoComplete="off"
                />
                <button
                  type="button"
                  className="button button-quiet"
                  onClick={() => void handlePickExecutable()}
                  disabled={formBusy || !onPickExecutable}
                  aria-label="选择可执行文件 Pick executable"
                >
                  {pickingExecutable ? '选择中…' : '浏览 Browse'}
                </button>
              </div>
              <small className="field-help">以项目路径为初始目录，只会选择 .exe、.bat 或 .cmd。直接运行所选文件，不分配端口或等待网页就绪。</small>
            </div>

            <label className="field field-span-two">
              <span>启动参数 <small>Arguments</small></span>
              <input
                aria-label="启动参数 Arguments"
                value={value.arguments}
                onChange={(event) => update('arguments', event.target.value)}
                placeholder="可选，例如 --profile dev"
                autoComplete="off"
              />
            </label>
          </>
        )}

        <label className="field">
          <span>图标 <small>Optional icon</small></span>
          <input
            value={value.icon ?? ''}
            onChange={(event) => update('icon', event.target.value)}
            placeholder="用一个 emoji 即可"
            maxLength={4}
            autoComplete="off"
          />
        </label>

        {launchMode === 'browser' ? (
          <label className="checkbox-field">
            <input
              type="checkbox"
              checked={value.autoOpenBrowser}
              onChange={(event) => update('autoOpenBrowser', event.target.checked)}
              disabled={submitting}
            />
            <span>启动后打开浏览器 <small>Open browser after start</small></span>
          </label>
        ) : null}
      </fieldset>

      <div className="dialog-actions">
        {onRemove ? (
          <button type="button" className="button button-danger button-quiet" onClick={onRemove} disabled={formBusy}>
            移除项目 Remove
          </button>
        ) : null}
        <span className="actions-spacer" />
        <button type="button" className="button button-quiet" onClick={onClose} disabled={submitting}>
          取消 Cancel
        </button>
        <button type="submit" className="button button-primary" disabled={formBusy}>
          {submitting ? '保存中…' : detecting ? '识别中…' : pickingExecutable ? '选择文件…' : submitLabel}
        </button>
      </div>
    </form>
  );
}
