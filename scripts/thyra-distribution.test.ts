import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

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
    for (const asset of [...OFFICIAL_ASSETS, "SHA256SUMS"]) {
      expect(release).toContain(`assets/${asset}`);
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
