import { describe, expect, it } from "vitest";
import { footerCompanyLinks, headerNavigation, site } from "./site";

describe("public GitHub links", () => {
  it("links the public repository from the header and footer", () => {
    expect(site.githubUrl).toBe("https://github.com/DakiaMail/dakia-desktop");
    expect(headerNavigation).toContainEqual(["GitHub", site.githubUrl]);
    expect(footerCompanyLinks).toContainEqual(["GitHub", site.githubUrl]);
  });
});
