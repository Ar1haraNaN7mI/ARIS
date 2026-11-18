import { useState } from "react";
import { ChatWorkspace } from "./ChatWorkspace";
import { TeamWorkspace } from "./TeamWorkspace";
import type { Bootstrap, Profile } from "./types";

export function App({ initialData, latestProfile }: { initialData?: Bootstrap; latestProfile?: Profile }) {
  const [chat, setChat] = useState(false);
  return chat ? <ChatWorkspace initialData={initialData} latestProfile={latestProfile} onTeams={() => setChat(false)} />
    : <TeamWorkspace initialData={initialData} latestProfile={latestProfile} onChat={() => setChat(true)} />;
}
