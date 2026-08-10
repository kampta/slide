import { describe, expect, it } from "vitest";
import { artifactKind } from "./ArtifactDock";

describe("artifactKind", () => {
  it("routes supported MIME families to safe previews", () => {
    expect(artifactKind("image/png")).toBe("image");
    expect(artifactKind("video/mp4")).toBe("video");
    expect(artifactKind("audio/mpeg")).toBe("audio");
    expect(artifactKind("application/pdf")).toBe("document");
  });
});
