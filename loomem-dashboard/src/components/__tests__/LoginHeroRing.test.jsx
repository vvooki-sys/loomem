// Cycle/171 — smoke: the login hero mark matches the live loomem.ai hero —
// three ramp-C gradient rings (r=77/56/36) in independently animated layers.

import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import LoginHeroRing from "../LoginHeroRing";

describe("LoginHeroRing — cycle /171", () => {
  it("renders three 3D ring layers with ramp-C gradient strokes", () => {
    render(<LoginHeroRing size={120} />);
    const wrap = screen.getByTestId("login-hero-ring");
    expect(wrap).toHaveClass("mark3d");
    const layers = wrap.querySelectorAll("svg.m3");
    expect(layers.length).toBe(3);
    expect(wrap.querySelector(".m3-r1 circle")).toHaveAttribute("r", "77");
    expect(wrap.querySelector(".m3-r2 circle")).toHaveAttribute("r", "56");
    expect(wrap.querySelector(".m3-r3 circle")).toHaveAttribute("r", "36");
    // each layer strokes with its own ramp-C gradient
    expect(wrap.querySelectorAll("linearGradient").length).toBe(3);
    expect(wrap.querySelectorAll("linearGradient stop").length).toBe(15);
  });
});
