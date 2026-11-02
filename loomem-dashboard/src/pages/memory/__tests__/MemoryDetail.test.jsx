import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import MemoryDetail from "../MemoryDetail";

it("keeps a pending save guarded when updated content arrives", async () => {
  let finish;
  const onEdit = vi.fn(() => new Promise((resolve) => { finish = resolve; }));
  const memory = { id: "m1", content: "first", layer: "L0" };
  const { rerender } = render(<MemoryDetail memory={memory} onEdit={onEdit} />);
  await userEvent.click(screen.getByRole("button", { name: "Edit" }));
  await userEvent.click(screen.getByRole("button", { name: "Save" }));
  rerender(<MemoryDetail memory={{ ...memory, content: "refreshed" }} onEdit={onEdit} />);
  await userEvent.click(screen.getByRole("button", { name: "Edit" }));
  expect(screen.getByRole("button", { name: "Saving…" })).toBeDisabled();
  expect(onEdit).toHaveBeenCalledTimes(1);
  await act(async () => finish());
});
