(ns jepsen.readyset.workloads
  "ReadySet workloads.

  Workloads are represented as maps with the following structure:

  `{:tables ; list of honeysql :create-table maps
    :queries
    {:query-id-one
     {:query ; honeysql :select map
      :expected-results ; fn from rows map (from table kw to list of rows) to
                          expected results for this query
      :gen-write ; fn from rows map to generated honeysql query maps for writes
     }}``")

(def votes
  {:tables
   [{:create-table :stories
     :with-columns [[:id :serial]
                    [:title :text]
                    [[:primary-key :id]]]}
    {:create-table :votes
     :with-columns [[:story-id :int]
                    [:user-id :int]]}]

   :queries
   {:votes
    {:query
     {:select [:id :title :vcount]
      :from [:stories]
      :left-join
      [[{:select [:story-id [:%count.* :vcount]]
         :from [:votes]
         :group-by :story-id}
        :vote-count]
       [:= :stories.id :vote-count.story-id]]
      :order-by [:stories.id]}
     :expected-results
     (fn compute-votes [rows]
       (let [vote-counts
             (->> rows
                  :votes
                  (group-by :votes/story-id)
                  (map
                   (fn [[story-id rows]]
                     [story-id (count rows)]))
                  (into {}))]
         (->> rows
              :stories
              (map #(select-keys % [:stories/id :stories/title]))
              (map (fn [{:stories/keys [id] :as story}]
                     (assoc story :vcount (get vote-counts id))))
              (sort-by :stories/id))))}}

   :gen-write
   (fn [rows]
     (let [write-candidate-tables (if (seq (:stories rows))
                                    [:stories :votes]
                                    [:stories])
           delete-candidate-tables (->> rows (filter (comp seq val)) (map key))
           candidates (concat
                       (map (partial vector :insert) write-candidate-tables)
                       (map (partial vector :delete) delete-candidate-tables))]
       (case (rand-nth candidates)
         [:insert :stories]
         {:insert-into :stories
          :columns [:title]
          ;; generate between 1 and 5 stories
          :values (for [_ (range 0 (inc (rand-int 5)))]
                    [(str "story-" (rand-int (Integer/MAX_VALUE)))])}

         [:insert :votes]
         (let [candidate-stories (->> rows :stories (map :stories/id))]
           {:insert-into :votes
            :columns [:story-id :user-id]
            ;; generate between 1 and 5 votes
            :values (for [_ (range 0 (inc (rand-int 5)))]
                      [(rand-nth candidate-stories)
                       (rand-int Integer/MAX_VALUE)])})

         [:delete :stories]
         ;; delete between 1 and 5 stories
         (let [ids-to-delete (->> rows
                                  :stories
                                  (map :stories/id)
                                  shuffle
                                  (take (inc (rand-int 5))))]
           {:delete-from :stories
            :where (if (= 1 (count ids-to-delete))
                     [:= :id (first ids-to-delete)]
                     [:in :id ids-to-delete])})

         [:delete :votes]
         ;; delete 1 vote
         (let [vote-to-delete (rand-nth (:votes rows))]
           {:delete-from :votes
            :where [:and
                    [:= :story-id (:votes/story-id vote-to-delete)]
                    [:= :user-id (:votes/user-id vote-to-delete)]]}))))})

(comment
  (def sample-rows
    {:stories [#:stories{:id 1 :title "a"}
               #:stories{:id 2 :title "b"}]
     :votes [#:votes{:story-id 1 :user-id 1}
             #:votes{:story-id 1 :user-id 2}]})

  (def sample-writes
    (for [_ (range 10)]
      ((:gen-write votes) sample-rows)))
  )