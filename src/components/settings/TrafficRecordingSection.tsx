import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { Label } from "@/components/ui/label";
import { settingsApi } from "@/lib/api";
import { useSettingsQuery } from "@/lib/query";
import { toast } from "@/lib/toast";
import type { TrafficRecordingConfig } from "@/types";

export function TrafficRecordingSection() {
  const { t } = useTranslation();
  const { data } = useSettingsQuery();
  const client = useQueryClient();
  const [busy, setBusy] = useState(false);
  const config = data?.trafficRecording ?? { enabled: false };

  const update = async (next: TrafficRecordingConfig) => {
    await settingsApi.setTrafficRecordingConfig(next);
    await client.invalidateQueries({ queryKey: ["settings"] });
  };

  const run = async (operation: () => Promise<void>) => {
    setBusy(true);
    try {
      await operation();
    } catch (error) {
      toast.error(t("settings.trafficRecording.failed"), {
        description: String(error),
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-4">
        <Label htmlFor="traffic-recording-enabled">
          {t("settings.trafficRecording.enabled")}
        </Label>
        <Switch
          id="traffic-recording-enabled"
          disabled={busy || !data}
          checked={config.enabled}
          onCheckedChange={(enabled) =>
            void run(() => update({ ...config, enabled }))
          }
        />
      </div>
      <p className="text-sm text-fg-3">
        {t("settings.trafficRecording.description")}
      </p>
      <p className="break-all text-sm text-fg-3">
        {config.directory || t("settings.trafficRecording.defaultDirectory")}
      </p>
      <div className="flex flex-wrap gap-2">
        <Button
          variant="neutral"
          size="compact"
          disabled={busy || !data}
          onClick={() =>
            void run(async () => {
              const directory = await settingsApi.pickDirectory(
                config.directory ?? undefined,
              );
              if (directory) await update({ ...config, directory });
            })
          }
        >
          {t("settings.trafficRecording.chooseDirectory")}
        </Button>
        <Button
          variant="neutral"
          size="compact"
          disabled={busy || !data}
          onClick={() =>
            void run(() => settingsApi.openTrafficRecordingDirectory())
          }
        >
          {t("settings.trafficRecording.openDirectory")}
        </Button>
        <Button
          variant="neutral"
          size="compact"
          disabled={busy || !data}
          onClick={() =>
            void run(async () => {
              const target = await settingsApi.saveFileDialog(
                `cc-switch-traffic-${new Date().toISOString().slice(0, 10)}.zip`,
              );
              if (!target) return;
              const result = await settingsApi.exportTrafficRecordings(target);
              toast.success(t("settings.trafficRecording.exported"), {
                description: t("settings.trafficRecording.exportSummary", {
                  count: result.requestCount,
                  incomplete: result.incompleteCount,
                }),
              });
            })
          }
        >
          {t("settings.trafficRecording.export")}
        </Button>
      </div>
    </div>
  );
}
