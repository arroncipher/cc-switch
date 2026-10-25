import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { TrafficRecordingSection } from "@/components/settings/TrafficRecordingSection";

const mocks = vi.hoisted(() => ({
  setTrafficRecordingConfig: vi.fn(),
  pickDirectory: vi.fn(),
  openTrafficRecordingDirectory: vi.fn(),
  saveFileDialog: vi.fn(),
  exportTrafficRecordings: vi.fn(),
  invalidateQueries: vi.fn(),
  success: vi.fn(),
  error: vi.fn(),
}));
vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("@/lib/api", () => ({ settingsApi: mocks }));
vi.mock("@/lib/query", () => ({
  useSettingsQuery: () => ({
    data: { trafficRecording: { enabled: false, directory: "/recordings" } },
  }),
}));
vi.mock("@tanstack/react-query", () => ({
  useQueryClient: () => ({ invalidateQueries: mocks.invalidateQueries }),
}));
vi.mock("@/lib/toast", () => ({ toast: mocks }));
const button = (suffix: string) =>
  screen.getByRole("button", { name: `settings.trafficRecording.${suffix}` });

describe("TrafficRecordingSection", () => {
  beforeEach(() => {
    vi.resetAllMocks();
  });
  it("defaults off and persists through the dedicated command", async () => {
    render(<TrafficRecordingSection />);
    expect(screen.getByRole("switch")).not.toBeChecked();
    fireEvent.click(screen.getByRole("switch"));
    await waitFor(() =>
      expect(mocks.setTrafficRecordingConfig).toHaveBeenCalledWith({
        enabled: true,
        directory: "/recordings",
      }),
    );
    expect(mocks.invalidateQueries).toHaveBeenCalledWith({
      queryKey: ["settings"],
    });
  });
  it("keeps the switch unchanged when saving fails", async () => {
    mocks.setTrafficRecordingConfig.mockRejectedValue(new Error("disk full"));
    render(<TrafficRecordingSection />);
    fireEvent.click(screen.getByRole("switch"));
    await waitFor(() => expect(mocks.error).toHaveBeenCalled());
    expect(screen.getByRole("switch")).not.toBeChecked();
    expect(mocks.invalidateQueries).not.toHaveBeenCalled();
  });
  it("persists a selected directory and ignores cancellation", async () => {
    mocks.pickDirectory
      .mockResolvedValueOnce(null)
      .mockResolvedValueOnce("/chosen");
    render(<TrafficRecordingSection />);
    fireEvent.click(button("chooseDirectory"));
    await waitFor(() => expect(button("chooseDirectory")).toBeEnabled());
    expect(mocks.setTrafficRecordingConfig).not.toHaveBeenCalled();
    fireEvent.click(button("chooseDirectory"));
    await waitFor(() =>
      expect(mocks.setTrafficRecordingConfig).toHaveBeenCalledWith({
        enabled: false,
        directory: "/chosen",
      }),
    );
    expect(mocks.pickDirectory).toHaveBeenCalledWith("/recordings");
  });
  it("exports a ZIP to the selected path and reports incomplete captures", async () => {
    mocks.saveFileDialog.mockResolvedValue("/export.zip");
    mocks.exportTrafficRecordings.mockResolvedValue({
      path: "/export.zip",
      requestCount: 2,
      incompleteCount: 1,
    });
    render(<TrafficRecordingSection />);
    fireEvent.click(button("export"));
    await waitFor(() => expect(mocks.success).toHaveBeenCalled());
    expect(mocks.exportTrafficRecordings).toHaveBeenCalledWith("/export.zip");
    expect(mocks.saveFileDialog).toHaveBeenCalledWith(
      expect.stringMatching(/^cc-switch-traffic-.*\.zip$/),
    );
  });
  it("cancels export and opens the configured directory", async () => {
    mocks.saveFileDialog.mockResolvedValue(null);
    render(<TrafficRecordingSection />);
    fireEvent.click(button("export"));
    await waitFor(() => expect(button("export")).toBeEnabled());
    expect(mocks.exportTrafficRecordings).not.toHaveBeenCalled();
    fireEvent.click(button("openDirectory"));
    await waitFor(() =>
      expect(mocks.openTrafficRecordingDirectory).toHaveBeenCalled(),
    );
  });
  it("reports export failure without claiming success", async () => {
    mocks.saveFileDialog.mockResolvedValue("/export.zip");
    mocks.exportTrafficRecordings.mockRejectedValue(
      new Error("permission denied"),
    );
    render(<TrafficRecordingSection />);
    fireEvent.click(button("export"));
    await waitFor(() => expect(mocks.error).toHaveBeenCalled());
    expect(mocks.success).not.toHaveBeenCalled();
  });
});
