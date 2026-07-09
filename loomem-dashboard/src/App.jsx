import { BrowserRouter, Routes, Route, Navigate } from "react-router-dom";
import AuthGate from "./components/AuthGate";
import Layout from "./components/Layout";
import ConnectPage from "./pages/ConnectPage";
import MemoryPage from "./pages/MemoryPage";
import SettingsPage from "./pages/SettingsPage";

// v1 shell: token unlock → Connect (first-run landing) / Memory / Settings.
export default function App() {
  return (
    <BrowserRouter>
      <AuthGate>
        {(userCtx) => (
          <Routes>
            <Route element={<Layout userCtx={userCtx} />}>
              <Route index element={<Navigate to="/connect" replace />} />
              <Route path="/connect" element={<ConnectPage userCtx={userCtx} />} />
              <Route path="/memory" element={<MemoryPage userCtx={userCtx} />} />
              <Route path="/settings" element={<SettingsPage userCtx={userCtx} />} />
              <Route path="*" element={<Navigate to="/connect" replace />} />
            </Route>
          </Routes>
        )}
      </AuthGate>
    </BrowserRouter>
  );
}
