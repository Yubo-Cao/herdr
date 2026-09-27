import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const workflow: any = Bun.YAML.parse(
  readFileSync(new URL("../.github/workflows/thyra-distribution.yml", import.meta.url), "utf8"),
);
const official: any = Bun.YAML.parse(
  readFileSync(new URL("../.github/workflows/release.yml", import.meta.url), "utf8"),
);
const cargoVersion = /^version = "([^"]+)"/m.exec(
  readFileSync(new URL("../Cargo.toml", import.meta.url), "utf8"),
)![1];
const [major, minor, patch] = cargoVersion.split(".").map(Number);
const OFFICIAL_ASSETS = [
  "herdr-linux-x86_64",
  "herdr-linux-aarch64",
  "herdr-macos-x86_64",
  "herdr-macos-aarch64",
  "herdr-windows-x86_64.zip",
];

describe("Thyra distribution workflow", () => {
  test("publishes only from fork tags and never from the official repository", () => {
    expect(workflow.on.push).toEqual({ tags: ["v*-thyra.*"] });
    expect(workflow.permissions).toEqual({ contents: "read" });
    expect(workflow.jobs.validate.if).toBe("github.repository != 'herdrdev/herdr'");
    const publish = workflow.jobs.publish;
    expect(publish.if).toContain("github.repository != 'herdrdev/herdr'");
    expect(publish.if).toContain("github.event_name == 'push'");
    expect(publish.permissions).toEqual({ contents: "write" });
    expect(publish.steps[0].run).toContain('"$GITHUB_ACTOR" "$GITHUB_TRIGGERING_ACTOR"');
  });

  test("builds the same targets and asset names as the official release", () => {
    const names = (jobs: any) =>
      jobs.build.strategy.matrix.include.map((entry: any) => `${entry.target} ${entry.name}`);
    expect(names(workflow.jobs)).toEqual(names(official.jobs));
    expect(workflow.jobs.build.strategy.matrix.include.map((entry: any) => entry.name)).toEqual(
      OFFICIAL_ASSETS,
    );
    const release = workflow.jobs.publish.steps.at(-1).run;
    for (const asset of [...OFFICIAL_ASSETS, "SHA256SUMS", "latest.json"]) {
      expect(release).toContain(`assets/${asset}`);
    }
  });

  test("generates the update manifest from the downloaded release assets", () => {
    const steps = workflow.jobs.publish.steps;
    const manifest = steps.findIndex((step: any) => step.name === "Write latest.json");
    expect(manifest).toBeGreaterThan(steps.findIndex((step: any) => step.name === "Write SHA256SUMS"));
    expect(manifest).toBeLessThan(steps.findIndex((step: any) => step.name === "Create GitHub release"));
    expect(steps[manifest].run).toBe(
      'python3 -m scripts.thyra_distribution --repo "$GITHUB_REPOSITORY" --tag "$GITHUB_REF_NAME" --assets ../assets',
    );
    expect(steps[manifest]["working-directory"]).toBe("source");
    const checkout = steps.slice(0, manifest).find((step: any) => step.uses?.startsWith("actions/checkout@"));
    expect(checkout.with.path).toBe("source"); // Keep source assets separate from release artifacts.
  });

  test("embeds the full fork version for updates and preserves dispatch builds", () => {
    expect(workflow.jobs.build.env.HERDR_VERSION).toBe("${{ needs.validate.outputs.version }}");
    const dir = mkdtempSync(join(tmpdir(), "herdr-thyra-version-"));
    try {
      for (const [refType, refName, version] of [
        ["tag", `v${cargoVersion}-thyra.12`, `${cargoVersion}-thyra.12`],
        ["branch", "thyra-distribution", cargoVersion],
      ]) {
        const output = join(dir, refType);
        const result = Bun.spawnSync(["bash", "-c", workflow.jobs.validate.steps[1].run], {
          env: { ...process.env, GITHUB_REF_TYPE: refType, GITHUB_REF_NAME: refName, GITHUB_OUTPUT: output },
          cwd: new URL("..", import.meta.url).pathname,
        });
        expect(result.exitCode).toBe(0);
        expect(readFileSync(output, "utf8")).toBe(`version=${version}\n`);
      }
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("tags must carry the Cargo version", () => {
    const check = workflow.jobs.validate.steps[1].run;
    for (const [tag, ok] of [
      [`v${cargoVersion}-thyra.1`, true],
      [`v${cargoVersion}-thyra.12`, true],
      [`v${major}.${minor}.${patch + 1}-thyra.1`, false],
      [`v${cargoVersion}`, false],
      [`v${cargoVersion}-thyra`, false],
    ] as const) {
      const result = Bun.spawnSync(["bash", "-c", check], {
        env: {
          ...process.env,
          GITHUB_REF_TYPE: "tag",
          GITHUB_REF_NAME: tag,
          GITHUB_OUTPUT: "/dev/null",
        },
        cwd: new URL("..", import.meta.url).pathname,
      });
      expect(result.exitCode === 0, tag).toBe(ok);
    }
  });
});
