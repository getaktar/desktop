import { Button, Input, Select, Text } from "@fluentui/react-components";
import { CopyRegular, FolderOpenRegular, OpenRegular } from "@fluentui/react-icons";
import { open } from "@tauri-apps/plugin-dialog";
import { useEffect, useState } from "react";

import { Card, CardRow, SettingsSection } from "../../components/SettingsLayout";
import { api, type CaptureToolStatus, type Settings } from "../../lib/api";
import { useI18n } from "../../lib/i18n";
import { isLinux } from "../../lib/platform";

type Translate = ReturnType<typeof useI18n>["t"];

/** The tools Capture can use here (the Mac app's docs/capture-tools.md),
 * looked up again whenever the Settings window comes back to the front. */
export function useCaptureTools() {
  const [tools, setTools] = useState<CaptureToolStatus[] | null>(null);
  useEffect(() => {
    const load = () => api.captureTools().then(setTools).catch(() => setTools([]));
    load();
    // A tool installed while Settings was open shows up when it's back.
    window.addEventListener("focus", load);
    return () => window.removeEventListener("focus", load);
  }, []);
  return tools;
}

/** "Capture with": which screenshot tool Capture runs, and what the
 * picked one needs (installing it, its save folder, or its setup). */
export function CaptureToolsSection({ settings, tools }: { settings: Settings; tools: CaptureToolStatus[] | null }) {
  const { t } = useI18n();
  if (!tools || tools.length === 0) return null;
  const selected = tools.find((tool) => tool.id === settings.captureTool) ?? tools[0];
  // Installed tools first, after the system's own.
  const ordered = [...tools].sort((a, b) => Number(a.id !== "system") - Number(b.id !== "system") || Number(b.installed) - Number(a.installed));

  return (
    <SettingsSection title={t("Capture")}>
      <Card>
        <CardRow title={t("Capture with")} subtitle={toolDescription(selected, t)}>
          <Select value={selected.id} onChange={(_, data) => api.updateSettings({ captureTool: data.value as CaptureToolStatus["id"] })}>
            {ordered.map((tool) => (
              <option key={tool.id} value={tool.id}>
                {toolLabel(tool, t)}
              </option>
            ))}
          </Select>
        </CardRow>
        {!selected.installed && (
          <CardRow title={t("{0} isn’t installed", selected.name)} subtitle={t("Until it is, Capture uses the system’s screenshot tool.")}>
            <Button icon={<OpenRegular />} onClick={() => api.openUrl(selected.website)}>
              {t("Get {0}", selected.name)}
            </Button>
          </CardRow>
        )}
        {selected.kind === "wait" && <SaveFolderRow tool={selected} folder={settings.captureFolder ?? null} />}
        {selected.kind === "send" && <SendSetup tool={selected} />}
      </Card>
      {selected.id === "spectacle" && (
        <Text size={200} className="secondary break">
          {t("Turn off Spectacle’s automatic copy and save after taking a screenshot, so the image that goes up is the annotated one.")}
        </Text>
      )}
    </SettingsSection>
  );
}

function toolLabel(tool: CaptureToolStatus, t: Translate) {
  if (tool.id === "system") return isLinux ? t("Desktop’s screenshot tool") : "Snipping Tool";
  return tool.installed ? tool.name : t("{0} (not installed)", tool.name);
}

function toolDescription(tool: CaptureToolStatus, t: Translate) {
  switch (tool.id) {
    case "system":
      return isLinux
        ? t("Your desktop’s own screenshot dialog. No editing: pick another tool to annotate before uploading.")
        : t("Windows’ snipping bar. No editing: pick another tool to annotate before uploading.");
    case "flameshot":
      return t("Select an area and annotate it in Flameshot. Accept (Enter) uploads it and copies its link; Esc cancels.");
    case "satty":
      return t("Take the screenshot with your desktop’s dialog, then edit it in {0}. Saving (Ctrl+S or Enter) uploads it; closing without saving cancels.", tool.name);
    case "swappy":
      return t("Take the screenshot with your desktop’s dialog, then edit it in {0}. Closing its window uploads the image.", tool.name);
    case "spectacle":
      return t("Select an area and annotate it in Spectacle, then copy or save it: Aktar uploads it. Aktar waits up to 10 minutes; Stop Waiting in the tray menu ends it.");
    case "sharex":
      return t("Capture starts ShareX’s region capture, and ShareX’s own shortcuts work too. ShareX annotates the screenshot and sends it to Aktar, once it’s set up as below.");
    default:
      return t("Use {0} with its own shortcuts and send the edited image to Aktar, once it’s set up as below. Aktar’s Capture uses the system’s screenshot tool.", tool.name);
  }
}

function SaveFolderRow({ tool, folder }: { tool: CaptureToolStatus; folder: string | null }) {
  const { t } = useI18n();
  const shown = folder ?? tool.defaultFolder;
  return (
    <CardRow title={t("Save folder")} subtitle={shown ?? t("Not set: only an image copied in {0} is uploaded.", tool.name)}>
      <div className="row-buttons">
        <Button
          icon={<FolderOpenRegular />}
          onClick={async () => {
            const selection = await open({ directory: true, multiple: false, defaultPath: shown ?? undefined });
            if (typeof selection === "string") api.updateSettings({ captureFolder: selection });
          }}
        >
          {t("Choose…")}
        </Button>
        {folder && <Button onClick={() => api.updateSettings({ captureFolder: "" })}>{t("Use {0}’s Folder", tool.name)}</Button>}
      </div>
    </CardRow>
  );
}

/** The steps to make a tool send its images to Aktar, with the program
 * and arguments to paste into it. */
function SendSetup({ tool }: { tool: CaptureToolStatus }) {
  const { t } = useI18n();
  if (!tool.uploadProgram || !tool.uploadArguments) return null;
  const [programLabel, argumentsLabel] = tool.id === "greenshot" ? [t("Command"), t("Argument")] : [t("Program"), t("Arguments")];
  return (
    <>
      <CardRow title={t("Set up {0}", tool.name)} subtitle={<SetupSteps tool={tool} t={t} />} />
      <CopyRow label={programLabel} value={tool.uploadProgram} />
      <CopyRow label={argumentsLabel} value={tool.uploadArguments} />
    </>
  );
}

function SetupSteps({ tool, t }: { tool: CaptureToolStatus; t: Translate }) {
  const steps =
    tool.id === "sharex"
      ? [
          t("In ShareX, open Task settings > Actions and add an action named Aktar, with the program and arguments below as File path and Arguments. Turn on Hidden window."),
          t("In After capture tasks, turn on Annotate image, Save image to file and Perform actions (with Aktar checked under it), and turn off Upload image to host."),
        ]
      : tool.id === "greenshot"
        ? [
            t("In Greenshot’s Preferences, open Plugins, select External command Plugin and choose Configure."),
            t("Add a command named Aktar, with the command and argument below."),
            t("In the editor, send the image to Aktar from the external command button, or pick Aktar under Preferences > Destination to send every capture."),
          ]
        : [t("In {0}’s settings, add an external program with the program and arguments below (in Snagit Editor: Share > Program), then share edited images to it.", tool.name)];
  return (
    <ol className="setup-steps">
      {steps.map((step) => (
        <li key={step}>{step}</li>
      ))}
    </ol>
  );
}

function CopyRow({ label, value }: { label: string; value: string }) {
  const { t } = useI18n();
  return (
    <CardRow title={label}>
      <div className="row-buttons">
        <Input readOnly value={value} className="command-field" aria-label={label} />
        <Button icon={<CopyRegular />} aria-label={t("Copy")} title={t("Copy")} onClick={() => api.copyText(value)} />
      </div>
    </CardRow>
  );
}
